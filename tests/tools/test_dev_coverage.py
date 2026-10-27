"""The frontend coverage report's paths, as tools/dev.py rewrites them for
SonarQube."""

from pathlib import Path
import sys
import tempfile
import unittest


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


if __name__ == "__main__":
    unittest.main()
