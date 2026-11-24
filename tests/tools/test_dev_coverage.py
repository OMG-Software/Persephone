"""The frontend and Rust coverage reports' paths, as tools/dev.py rewrites
them for SonarQube, and the cargo-llvm-cov environment it reads."""

import io
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


def coverage(*arguments, platform="linux", skip=None):
    """Runs `dev.py coverage` with these arguments on `platform`, recording
    each command and its environment instead of starting it. `skip` is the
    reason the call worker is skipped, or None, so that no test depends on
    the clang of the machine it runs on."""
    commands = []

    def record(command, environment, *, cwd=dev.ROOT):
        commands.append(([str(part) for part in command], environment))

    with (
        # Without CI's PERSEPHONE_CALL_WORKER=required, which check.yml sets
        # for all jobs, so that `skip` decides.
        mock.patch.dict(
            dev.os.environ,
            {
                **{name: value for name, value in dev.os.environ.items() if name != "PERSEPHONE_CALL_WORKER"},
                "XDG_RUNTIME_DIR": "/run/user/1000",
            },
            clear=True,
        ),
        mock.patch.object(sys, "argv", ["dev.py", "coverage", *arguments]),
        mock.patch.object(sys, "platform", platform),
        mock.patch.object(dev, "run", record),
        mock.patch.object(dev, "coverage_environment", lambda environment: {**environment, "CARGO_LLVM_COV": "1"}),
        mock.patch.object(dev, "repository_paths"),
        mock.patch.object(dev.webrtc, "ensure", return_value=Path("webrtc-root")),
        mock.patch.object(dev, "call_worker_skip", return_value=skip),
        mock.patch.object(dev.shutil, "which", return_value="npm"),
        mock.patch.object(dev.shutil, "copy2"),
    ):
        dev.main()
    return commands


class CallWorkerTest(unittest.TestCase):
    def test_given_a_usable_clang_then_coverage_builds_the_call_worker(self):
        commands = coverage()
        self.assertTrue([c for c, _ in commands if "persephone-call-worker" in c and c[1] == "build"])

    def test_given_a_clang_too_old_then_coverage_skips_only_the_call_worker(self):
        # GIVEN a reason to skip the call worker WHEN coverage runs
        with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as stderr:
            commands = coverage(skip="clang 18 is older than the clang 21 libwebrtc needs")
        # THEN it is not built, the reason is printed, and the instrumented tests still run
        self.assertFalse([c for c, _ in commands if c[1] == "build" and "persephone-call-worker" in c])
        self.assertIn("call worker skipped: clang 18 is older", stderr.getvalue())
        self.assertTrue([c for c, _ in commands if c[:2] == ["cargo", "test"]])


class LinuxLiveTest(unittest.TestCase):
    def test_given_no_linux_live_then_coverage_runs_no_ignored_test(self):
        commands = coverage()
        self.assertFalse([command for command, _ in commands if "--ignored" in command])

    def test_given_linux_live_then_the_live_tests_run_instrumented_before_the_report(self):
        with mock.patch.dict(dev.os.environ, {"DISPLAY": ":0", "WAYLAND_DISPLAY": "wayland-0"}):
            commands = coverage("--linux-live")
        names = [command for command, _ in commands]
        report = names.index(next(c for c in names if c[:3] == ["cargo", "llvm-cov", "report"]))
        live = [index for index, command in enumerate(names) if "--ignored" in command]
        self.assertEqual(len(live), 3)
        self.assertTrue(all(index < report for index in live))
        scope, unlocked, bare = (commands[index] for index in live)
        self.assertEqual(scope[0][-1], "platform::linux::ui::")
        self.assertEqual(scope[1]["CARGO_LLVM_COV"], "1", "instrumented")
        self.assertEqual(scope[1]["XDG_RUNTIME_DIR"], "/run/user/1000", "the user manager's")

        # THEN each Secret Service test gets a private bus, keyring data in
        # the ignored .local tree, and a new runtime directory, so it cannot
        # find the user's running keyring; both are removed after the run.
        for keyring in (unlocked, bare):
            self.assertEqual(keyring[0][:2], ["dbus-run-session", "--"])
            self.assertEqual(keyring[1]["CARGO_LLVM_COV"], "1", "instrumented")
            data = Path(keyring[1]["XDG_DATA_HOME"])
            self.assertEqual(data.parent, dev.LIVE_DATA)
            runtime = Path(keyring[1]["XDG_RUNTIME_DIR"])
            self.assertTrue(runtime.name.startswith("persephone-keyring-"))
            self.assertNotEqual(runtime, Path("/run/user/1000"))
            self.assertFalse(runtime.exists(), "removed after the run")
            self.assertFalse(data.exists(), "removed after the run")
        self.assertNotEqual(unlocked[1]["XDG_DATA_HOME"], bare[1]["XDG_DATA_HOME"], "a new keyring each")

        # THEN the stored-once test gets an unlocked default keyring, and the
        # ADR-0061 refusals get a keyring without one and no display, so the
        # keyring's own prompt can never appear on the user's desktop.
        vault = "platform::linux::vault::tests::"
        self.assertEqual(unlocked[0][-1], vault + "given_a_disposable_keyring_then")
        self.assertIn("--unlock", unlocked[0][4])
        self.assertEqual(bare[0][-1], vault + "given_a_disposable_provider_without_a_default")
        self.assertNotIn("--unlock", bare[0][4])
        self.assertNotIn("DISPLAY", bare[1])
        self.assertNotIn("WAYLAND_DISPLAY", bare[1])

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
