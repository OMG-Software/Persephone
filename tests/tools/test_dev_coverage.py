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


if __name__ == "__main__":
    unittest.main()
