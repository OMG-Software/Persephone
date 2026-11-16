"""The cargo and npm commands that `python tools/dev.py build` runs."""

from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools"))
import dev  # noqa: E402


def build(*arguments, commands=None, fails=lambda command: False, environments=None, library=Path("webrtc-root")):
    """Runs `dev.py` with these arguments and npm found, recording each
    command in `commands` (and its environment in `environments`) instead of
    starting it. A command for which `fails` is true exits with an error, as
    cargo would. The pinned libwebrtc is a stand-in at `library`, so no test
    downloads anything."""
    commands = [] if commands is None else commands
    environments = [] if environments is None else environments

    def record(command, environment, *, cwd=dev.ROOT):
        command = [str(part) for part in command]
        commands.append(command)
        environments.append(environment)
        if fails(command):
            raise subprocess.CalledProcessError(101, command)

    with mock.patch.object(sys, "argv", ["dev.py", *arguments]), mock.patch.object(dev, "run", record):
        with mock.patch.object(dev.shutil, "which", return_value="npm"):
            with mock.patch.object(dev.webrtc, "ensure", return_value=library):
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


if __name__ == "__main__":
    unittest.main()
