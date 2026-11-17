"""The cargo and npm commands that `python tools/dev.py build` runs."""

import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools"))
import dev  # noqa: E402


def call_worker_environment(required):
    """`os.environ` with PERSEPHONE_CALL_WORKER set only when `required`, so
    that the tests do not take CI's value, which check.yml sets for all jobs."""
    environment = {name: value for name, value in os.environ.items() if name != "PERSEPHONE_CALL_WORKER"}
    if required:
        environment["PERSEPHONE_CALL_WORKER"] = "required"
    return environment


def build(
    *arguments,
    commands=None,
    fails=lambda command: False,
    environments=None,
    library=Path("webrtc-root"),
    skip=None,
    required=False,
):
    """Runs `dev.py` with these arguments and npm found, recording each
    command in `commands` (and its environment in `environments`) instead of
    starting it. A command for which `fails` is true exits with an error, as
    cargo would. The pinned libwebrtc is a stand-in at `library`, so no test
    downloads anything. `skip` is the reason the call worker is skipped, or
    None, so that no test depends on the clang of the machine it runs on.
    `required` sets PERSEPHONE_CALL_WORKER=required."""
    commands = [] if commands is None else commands
    environments = [] if environments is None else environments

    def record(command, environment, *, cwd=dev.ROOT):
        command = [str(part) for part in command]
        commands.append(command)
        environments.append(environment)
        if fails(command):
            raise subprocess.CalledProcessError(101, command)

    with (
        mock.patch.dict(dev.os.environ, call_worker_environment(required), clear=True),
        mock.patch.object(sys, "argv", ["dev.py", *arguments]),
        mock.patch.object(dev, "run", record),
    ):
        with mock.patch.object(dev.shutil, "which", return_value="npm"):
            with mock.patch.object(dev.webrtc, "ensure", return_value=library):
                with mock.patch.object(dev, "call_worker_skip", return_value=skip):
                    dev.main()
    return commands


def packages(command):
    return [command[index + 1] for index, part in enumerate(command) if part == "-p"]


class BuildTest(unittest.TestCase):
    def test_given_build_then_the_frontend_and_guest_come_before_the_executables(self):
        commands = build("build")
        self.assertEqual(commands[0], ["npm", "run", "build"])
        self.assertEqual(packages(commands[1]), ["persephone-media-decoder"])
        self.assertIn("--release", commands[1], "the worker embeds the release guest in every profile")
        self.assertEqual(
            [packages(command) for command in commands[2:]],
            [["persephone-core", "persephone-media-worker"], ["persephone-call-worker"], ["persephone-desktop"]],
            "the UI and the call worker build apart, so their features are not unified into the core and worker",
        )

    def test_given_build_without_release_then_the_executables_use_the_debug_profile(self):
        for command in build("build")[2:]:
            self.assertNotIn("--release", command)

    def test_given_build_with_release_then_the_executables_use_the_release_profile(self):
        for command in build("build", "--release")[2:]:
            self.assertIn("--release", command)

    def test_given_release_with_another_command_then_nothing_runs(self):
        commands = []
        with mock.patch.object(sys, "stderr"), self.assertRaises(SystemExit) as stopped:
            build("check", "--release", commands=commands)
        self.assertEqual(stopped.exception.code, 2)
        self.assertEqual(commands, [])

    def test_given_a_failed_core_build_then_the_ui_is_not_built(self):
        commands = []
        with self.assertRaises(subprocess.CalledProcessError):
            build("build", commands=commands, fails=lambda command: "persephone-core" in command)
        self.assertEqual(packages(commands[-1]), ["persephone-core", "persephone-media-worker"])

    def test_given_build_then_the_call_worker_gets_the_verified_libwebrtc_path(self):
        # GIVEN a verified libwebrtc, WHEN build runs, THEN the worker build names it.
        commands, environments = [], []
        build("build", commands=commands, environments=environments, library=Path("verified-root"))
        index = next(i for i, command in enumerate(commands) if packages(command) == ["persephone-call-worker"])
        self.assertEqual(environments[index]["LK_CUSTOM_WEBRTC"], "verified-root")

    def test_given_an_unverified_libwebrtc_then_nothing_is_built(self):
        # GIVEN libwebrtc cannot be verified, WHEN build runs, THEN no command runs and it fails.
        commands = []
        with mock.patch.object(dev.webrtc, "ensure", side_effect=dev.webrtc.WebrtcError("bad hash")):
            with mock.patch.object(sys, "argv", ["dev.py", "build"]), mock.patch.object(dev, "run", lambda *a, **k: commands.append(a)):
                with mock.patch.object(dev.shutil, "which", return_value="npm"), mock.patch.object(sys, "stderr"):
                    self.assertEqual(dev.main(), 1)
        self.assertEqual(commands, [])

    def test_given_rust_then_the_call_worker_is_tested_apart_from_the_workspace(self):
        # GIVEN rust, WHEN it runs, THEN workspace commands exclude the call worker and it has its own test and lint.
        commands = build("rust")
        workspace = [c for c in commands if "--workspace" in c and c[1] in ("test", "clippy")]
        self.assertTrue(workspace)
        for command in workspace:
            self.assertIn("persephone-call-worker", command[command.index("--exclude") + 1 :])
        own = [c[1] for c in commands if packages(c) == ["persephone-call-worker"]]
        self.assertEqual(own, ["build", "test", "clippy"])


    def test_given_a_clang_too_old_then_the_call_worker_is_skipped_and_says_so(self):
        # GIVEN a reason to skip the call worker
        # (coverage has its own harness in test_dev_coverage.py)
        for command in ("build", "rust"):
            with self.subTest(command), mock.patch.object(sys, "stderr", new_callable=io.StringIO) as stderr:
                # WHEN the command runs
                commands = build(command, skip="clang 19 is older than the clang 21 libwebrtc needs")
                # THEN no command names the call worker, and the reason is printed
                self.assertFalse([c for c in commands if packages(c) == ["persephone-call-worker"]])
                self.assertIn("call worker skipped: clang 19 is older", stderr.getvalue())
                # AND the rest still runs
                self.assertTrue([c for c in commands if c[:2] == ["cargo", "build"]])


    def test_given_the_call_worker_is_required_then_a_skip_fails_before_anything_runs(self):
        # GIVEN PERSEPHONE_CALL_WORKER=required, as in CI, and a reason to skip
        commands = []
        with mock.patch.dict(dev.os.environ, {"PERSEPHONE_CALL_WORKER": "required"}):
            with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as stderr:
                with mock.patch.object(sys, "argv", ["dev.py", "rust"]), mock.patch.object(dev, "run", lambda *a, **k: commands.append(a)):
                    with mock.patch.object(dev.webrtc, "ensure", return_value=Path("webrtc-root")):
                        with mock.patch.object(dev, "call_worker_skip", return_value="clang 19 is older"):
                            # WHEN rust runs THEN it fails and runs nothing
                            self.assertEqual(dev.main(), 1)
        self.assertEqual(commands, [])
        self.assertIn("PERSEPHONE_CALL_WORKER is required", stderr.getvalue())

    def test_given_the_call_worker_is_required_and_buildable_then_it_is_built(self):
        commands = build("rust", required=True)
        self.assertEqual([c[1] for c in commands if packages(c) == ["persephone-call-worker"]], ["build", "test", "clippy"])


class CallWorkerSkipTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.library = Path(directory.name)

    def header(self, text):
        path = self.library / dev.LIBCXX_COMPILER_HEADER
        path.parent.mkdir(parents=True)
        path.write_text(text, encoding="utf-8")

    def compiler(self, stdout):
        result = subprocess.CompletedProcess([], 0, stdout=stdout, stderr="")
        return mock.patch.object(dev.subprocess, "run", return_value=result)

    def test_given_the_libcxx_header_then_its_minimum_clang_is_read(self):
        # GIVEN libc++ that rejects clang below 22.0 THEN clang 22 is needed
        self.header("#  if _LIBCPP_CLANG_VER < 2200\n#    warning old clang\n")
        self.assertEqual(dev.required_clang(self.library), 22)

    def test_given_no_header_or_no_minimum_then_the_fallback_is_used(self):
        self.assertEqual(dev.required_clang(self.library), dev.MIN_CALL_WORKER_CLANG)
        self.header("// nothing here\n")
        self.assertEqual(dev.required_clang(self.library), dev.MIN_CALL_WORKER_CLANG)

    def test_given_compiler_output_then_only_a_clang_major_is_read(self):
        with self.compiler("#define __GNUC__ 4\n#define __clang_major__ 19\n"):
            self.assertEqual(dev.clang_major({}), 19)
        with self.compiler("#define __GNUC__ 14\n"):
            self.assertIsNone(dev.clang_major({}), "GCC is not clang")
        with mock.patch.object(dev.subprocess, "run", side_effect=FileNotFoundError):
            self.assertIsNone(dev.clang_major({}))

    def test_given_cxx_then_that_compiler_is_asked(self):
        with self.compiler("#define __clang_major__ 21\n") as run:
            dev.clang_major({"CXX": "clang++-21 --flag"})
        self.assertEqual(run.call_args.args[0][:2], ["clang++-21", "--flag"])

    def test_given_each_clang_on_linux_then_only_an_old_or_missing_one_skips(self):
        self.header("#  if _LIBCPP_CLANG_VER < 2100\n")
        for found, skipped in ((19, True), (None, True), (21, False), (22, False)):
            with self.subTest(found=found), mock.patch.object(dev, "clang_major", return_value=found):
                reason = dev.call_worker_skip({}, self.library, platform="linux")
                self.assertEqual(reason is not None, skipped, reason)

    def test_given_windows_or_macos_then_the_call_worker_is_never_skipped(self):
        with mock.patch.object(dev, "clang_major", side_effect=AssertionError("not probed")):
            for platform in ("win32", "darwin"):
                self.assertIsNone(dev.call_worker_skip({}, self.library, platform=platform))


if __name__ == "__main__":
    unittest.main()
