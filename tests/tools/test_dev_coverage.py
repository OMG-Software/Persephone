"""The frontend and Rust coverage reports' paths, as tools/dev.py rewrites
them for SonarQube, and the cargo-llvm-cov environment it reads."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools"))
import dev  # noqa: E402


class RepositoryPathsTest(unittest.TestCase):
    def test_given_paths_relative_to_the_test_folder_then_they_become_repository_relative(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            frontend = root / "frontend"
            (frontend / "src/bridge").mkdir(parents=True)
            lcov = root / "lcov.info"
            lcov.write_text(
                "TN:\nSF:src\\bridge\\index.ts\nDA:1,1\nend_of_record\n"
                "SF:src/preferences.ts\nDA:2,0\nend_of_record\n",
                encoding="utf-8",
            )
            dev.repository_paths(lcov, frontend, root)
            self.assertEqual(
                lcov.read_text(encoding="utf-8"),
                "TN:\nSF:frontend/src/bridge/index.ts\nDA:1,1\nend_of_record\n"
                "SF:frontend/src/preferences.ts\nDA:2,0\nend_of_record\n",
            )

    def test_given_a_path_that_is_not_plain_and_relative_then_the_rewrite_refuses(self):
        for source in (
            "../../elsewhere.ts",
            "src/../../x.ts",
            "/etc/x.ts",
            "C:\\x.ts",
            "./src/x.ts",
            "src//x.ts",
            "src/x .ts",
        ):
            with self.subTest(source=source), tempfile.TemporaryDirectory() as directory:
                root = Path(directory) / "repository"
                (root / "frontend").mkdir(parents=True)
                lcov = root / "lcov.info"
                lcov.write_text(f"SF:{source}\n", encoding="utf-8")
                with self.assertRaises(ValueError):
                    dev.repository_paths(lcov, root / "frontend", root)
                self.assertEqual(lcov.read_text(encoding="utf-8"), f"SF:{source}\n", "left unchanged")

    def test_given_absolute_paths_in_the_repository_then_they_become_repository_relative(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            lcov = root / "lcov.info"
            windows = root.as_posix().replace("/", "\\")
            lcov.write_text(
                f"SF:{root.as_posix()}/crates/core/src/lib.rs\nDA:1,1\nend_of_record\n"
                f"SF:{windows.swapcase()}\\src-tauri\\src\\lib.rs\nDA:2,0\nend_of_record\n",
                encoding="utf-8",
            )
            dev.repository_paths(lcov, root, root, absolute=True)
            self.assertEqual(
                lcov.read_text(encoding="utf-8"),
                "SF:crates/core/src/lib.rs\nDA:1,1\nend_of_record\n"
                "SF:src-tauri/src/lib.rs\nDA:2,0\nend_of_record\n",
            )

    def test_given_an_absolute_path_outside_the_repository_then_the_rewrite_refuses(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "repository"
            root.mkdir()
            for source in (
                f"{Path(directory).as_posix()}/elsewhere/lib.rs",
                f"{root.as_posix()}-other/lib.rs",
                f"{root.as_posix()}/crates/../../x.rs",
                "crates/core/src/lib.rs",
            ):
                with self.subTest(source=source):
                    lcov = root / "lcov.info"
                    lcov.write_text(f"SF:{source}\n", encoding="utf-8")
                    with self.assertRaises(ValueError):
                        dev.repository_paths(lcov, root, root, absolute=True)
                    self.assertEqual(lcov.read_text(encoding="utf-8"), f"SF:{source}\n", "left unchanged")


def answers(version, shown):
    """A `subprocess.run` that answers `cargo llvm-cov --version` and
    `show-env` with this output."""

    def run(command, **_):
        output = version if command[-1] == "--version" else shown
        return subprocess.CompletedProcess(command, 0, stdout=output, stderr="")

    return run


class CoverageEnvironmentTest(unittest.TestCase):
    def test_given_show_env_output_then_its_variables_are_set_for_the_coverage_target(self):
        shown = (
            "LLVM_PROFILE_FILE='/r/.local/build/rust-coverage/p-%p-%16m.profraw'\n"
            "RUSTC_WRAPPER='/tools/cargo-llvm-cov'\n"
            "CARGO_LLVM_COV=1\n"
        )
        with mock.patch.object(dev.subprocess, "run", answers(f"cargo-llvm-cov {dev.LLVM_COV_VERSION}\n", shown)):
            environment = dev.coverage_environment({"PATH": "/bin"})
        self.assertEqual(environment["CARGO_TARGET_DIR"], str(dev.COVERAGE_TARGET))
        self.assertEqual(environment["LLVM_PROFILE_FILE"], "/r/.local/build/rust-coverage/p-%p-%16m.profraw")
        self.assertEqual(environment["RUSTC_WRAPPER"], "/tools/cargo-llvm-cov")
        self.assertEqual(environment["CARGO_LLVM_COV"], "1")
        self.assertEqual(environment["PATH"], "/bin")

    def test_given_another_cargo_llvm_cov_version_then_the_run_stops(self):
        with mock.patch.object(dev.subprocess, "run", answers("cargo-llvm-cov 0.1.0\n", "")):
            with self.assertRaises(SystemExit):
                dev.coverage_environment({})

    def test_given_show_env_output_that_is_not_plain_assignments_then_the_run_stops(self):
        for line in ("not an assignment", "BAD NAME='x'", "QUOTED='it's'", "export X='y'"):
            with self.subTest(line=line):
                version = f"cargo-llvm-cov {dev.LLVM_COV_VERSION}\n"
                with mock.patch.object(dev.subprocess, "run", answers(version, line + "\n")):
                    with self.assertRaises(ValueError):
                        dev.coverage_environment({})


def coverage(*arguments, platform="linux"):
    """Runs `dev.py coverage` with these arguments on `platform`, recording
    each command and its environment instead of starting it."""
    commands = []

    def record(command, environment, *, cwd=dev.ROOT):
        commands.append(([str(part) for part in command], environment))

    with (
        mock.patch.object(sys, "argv", ["dev.py", "coverage", *arguments]),
        mock.patch.object(sys, "platform", platform),
        mock.patch.object(dev, "run", record),
        mock.patch.object(dev, "coverage_environment", lambda environment: {**environment, "CARGO_LLVM_COV": "1"}),
        mock.patch.object(dev, "repository_paths"),
        mock.patch.object(dev.shutil, "which", return_value="npm"),
        mock.patch.object(dev.shutil, "copy2"),
    ):
        dev.main()
    return commands


class LinuxLiveTest(unittest.TestCase):
    def test_given_no_linux_live_then_coverage_runs_no_ignored_test(self):
        commands = coverage()
        self.assertFalse([command for command, _ in commands if "--ignored" in command])

    def test_given_linux_live_then_the_live_tests_run_instrumented_before_the_report(self):
        commands = coverage("--linux-live")
        names = [command for command, _ in commands]
        report = names.index(next(c for c in names if c[:3] == ["cargo", "llvm-cov", "report"]))
        live = [index for index, command in enumerate(names) if "--ignored" in command]
        self.assertEqual(len(live), 2)
        self.assertTrue(all(index < report for index in live))
        scope, keyring = (commands[index] for index in live)
        self.assertEqual(scope[0][-1], "platform::linux::ui::")
        self.assertEqual(scope[1]["CARGO_LLVM_COV"], "1", "instrumented")

        # THEN the Secret Service test gets a private bus and a keyring
        # directory of its own, inside the ignored .local tree.
        self.assertEqual(keyring[0][:2], ["dbus-run-session", "--"])
        self.assertEqual(keyring[0][-1], "platform::linux::vault::")
        self.assertEqual(keyring[1]["CARGO_LLVM_COV"], "1", "instrumented")
        data = Path(keyring[1]["XDG_DATA_HOME"])
        self.assertEqual(data.parent, dev.LIVE_DATA)
        self.assertFalse(data.exists(), "removed after the run")

    def test_given_linux_live_off_linux_or_outside_coverage_then_it_is_refused(self):
        for platform, command in (("win32", "coverage"), ("darwin", "coverage"), ("linux", "rust")):
            with self.subTest(platform=platform, command=command):
                with (
                    mock.patch.object(sys, "argv", ["dev.py", command, "--linux-live"]),
                    mock.patch.object(sys, "platform", platform),
                    mock.patch.object(dev, "run") as run,
                    mock.patch("sys.stderr"),
                ):
                    with self.assertRaises(SystemExit):
                        dev.main()
                run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
