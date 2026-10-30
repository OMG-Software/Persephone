"""The cargo and npm commands that `python tools/dev.py build` runs."""

from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools"))
import dev  # noqa: E402


def build(*arguments, commands=None, fails=lambda command: False):
    """Runs `dev.py` with these arguments and npm found, recording each
    command in `commands` instead of starting it. A command for which `fails`
    is true exits with an error, as cargo would."""
    commands = [] if commands is None else commands

    def record(command, environment, *, cwd=dev.ROOT):
        command = [str(part) for part in command]
        commands.append(command)
        if fails(command):
            raise subprocess.CalledProcessError(101, command)

    with mock.patch.object(sys, "argv", ["dev.py", *arguments]), mock.patch.object(dev, "run", record):
        with mock.patch.object(dev.shutil, "which", return_value="npm"):
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
            [["persephone-core", "persephone-media-worker"], ["persephone-desktop"]],
            "the UI builds apart from the core and worker, so its features are not unified into them",
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


if __name__ == "__main__":
    unittest.main()
