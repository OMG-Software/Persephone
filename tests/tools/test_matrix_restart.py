import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from tools import matrix_restart


class MatrixRestartHarnessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=matrix_restart.LOCAL_ROOT)
        self.root = Path(self.temp.name)
        self.profiles_path = self.root / "disposable-profiles.json"
        self.profiles_path.write_text(json.dumps(self.profiles()), encoding="utf-8")
        self.run_path = self.root / "run"
        self.binary = matrix_restart.LOCAL_ROOT / "fake-worker.exe"

    def tearDown(self):
        self.temp.cleanup()

    @staticmethod
    def profiles():
        return [
            {
                "name": "merovingian",
                "version": "1.0",
                "base_url": "http://127.0.0.1:18080",
                "username": "p0_alice",
                "password": "synthetic-alice-one",
                "recipient": {"username": "p0_bob", "password": "synthetic-bob-one"},
            },
            {
                "name": "synapse",
                "version": "1.1",
                "base_url": "http://[::1]:18081/",
                "username": "p0_alice",
                "password": "synthetic-alice-two",
                "recipient": {"username": "p0_bob", "password": "synthetic-bob-two"},
            },
        ]

    def fake_worker(self, calls, *, fail_phase=None, duplicate_pid=False, false_check_phase=None):
        def run(command, **kwargs):
            phase = command[3].removeprefix("--restart-")
            server = Path(command[1]).parent.name
            calls.append((command, kwargs, server, phase))
            store_root = Path(command[4])
            if phase == "seed":
                self.assertFalse(store_root.exists(), "seed must create the store root itself")
                store_root.mkdir()
            else:
                self.assertTrue(store_root.is_dir())
            checks = {
                "seed": {
                    "mutual_sas_and_bidirectional_native_e2ee": True,
                    "encrypted_session_checkpoints_read_back": True,
                    "no_token_key_or_message_markers_in_store_artifacts": True,
                    "native_rounds": 2,
                    "classic_rounds": 0,
                    "scanned_files": 4,
                },
                "wrong-key": {"both_wrong_keys_rejected": True},
                "missing-key": {"missing_keys_rejected_without_plaintext_fallback": True},
                "offline": {
                    "same_users_devices_keys_and_mutual_verification_before_sync": True,
                    "persisted_room_keys_decrypt_both_old_exact_events": True,
                    "password_login_and_sas_repeated": False,
                    "closed_origin_no_sync_or_history_requests": True,
                },
                "resume": {
                    "same_users_devices_keys_and_mutual_verification_before_sync": True,
                    "persisted_room_keys_decrypt_both_old_exact_events": True,
                    "password_login_and_sas_repeated": False,
                    "new_bidirectional_native_decrypted_sync_receipts": True,
                    "same_security_state_after_new_messages": True,
                    "no_token_key_or_message_markers_in_store_artifacts": True,
                    "both_logouts_confirmed": True,
                    "native_rounds": 3,
                    "classic_rounds": 0,
                    "scanned_files": 4,
                },
            }[phase]
            if false_check_phase == phase and server == "merovingian":
                checks["both_wrong_keys_rejected"] = False
            pid = 3000 + len(calls)
            if duplicate_pid and server == "merovingian" and phase == "wrong-key":
                pid = 3001
            report = {
                "server": server,
                "version": "1.0" if server == "merovingian" else "1.1",
                "phase": phase,
                "pid": pid,
                "passed": True,
                "checks": checks,
                "access_token": "must-not-be-copied-to-summary",
            }
            Path(command[2]).write_text(json.dumps(report), encoding="utf-8")
            if phase == fail_phase and server == "merovingian":
                return type("Completed", (), {"returncode": 7, "stdout": b"SECRET", "stderr": b"SECRET"})()
            return type("Completed", (), {"returncode": 0, "stdout": b"SECRET", "stderr": b"SECRET"})()

        return run

    def test_runs_distinct_process_phases_and_strips_resume_credentials(self):
        calls = []
        with patch.object(matrix_restart.subprocess, "run", side_effect=self.fake_worker(calls)):
            summary, status = matrix_restart.run_harness(
                self.profiles_path, self.run_path, binary=self.binary
            )

        self.assertEqual(status, 0)
        self.assertTrue(summary["passed"])
        self.assertEqual(len(calls), 10)
        for name in ("merovingian", "synapse"):
            server_dir = self.run_path / name
            seed = json.loads((server_dir / "seed-profile.json").read_text(encoding="utf-8"))[0]
            resume = json.loads((server_dir / "resume-profile.json").read_text(encoding="utf-8"))[0]
            self.assertEqual(set(resume), {"name", "version", "base_url"})
            self.assertIn("password", seed)
            server_calls = [call for call in calls if call[2] == name]
            self.assertEqual([call[3] for call in server_calls], [phase[0] for phase in matrix_restart.PHASES])
            received = {call[3]: call[1]["input"] for call in server_calls}
            self.assertEqual(len(received["seed"]), 64)
            self.assertEqual(received["offline"], received["seed"])
            self.assertEqual(received["resume"], received["seed"])
            self.assertEqual(len(received["wrong-key"]), 64)
            self.assertNotEqual(received["wrong-key"], received["seed"])
            self.assertEqual(received["missing-key"], b"")
            for command, kwargs, _, _ in server_calls:
                self.assertNotIn(received["seed"], [str(arg).encode() for arg in command])
                self.assertEqual(kwargs["timeout"], 180)
                self.assertTrue(kwargs["capture_output"])

        summary_text = (self.run_path / "summary.json").read_text(encoding="utf-8")
        self.assertNotIn("must-not-be-copied", summary_text)
        for server in summary["servers"]:
            for phase in server["phases"]:
                report_text = (self.run_path / server["server"] / f"{phase['phase']}.json").read_text(encoding="utf-8")
                self.assertNotIn("must-not-be-copied", report_text)
        pids = [phase["pid"] for server in summary["servers"] for phase in server["phases"]]
        self.assertEqual(len(pids), len(set(pids)))
        for server in summary["servers"]:
            phase_checks = {entry["phase"]: entry["checks"] for entry in server["phases"]}
            self.assertTrue(phase_checks["wrong-key"]["both_wrong_keys_rejected"])
            self.assertTrue(phase_checks["missing-key"]["missing_keys_rejected_without_plaintext_fallback"])

    def test_failed_wrong_key_stops_that_server_and_summary_is_sanitized(self):
        calls = []
        with patch.object(
            matrix_restart.subprocess,
            "run",
            side_effect=self.fake_worker(calls, fail_phase="wrong-key"),
        ):
            summary, status = matrix_restart.run_harness(
                self.profiles_path, self.run_path, binary=self.binary
            )

        self.assertEqual(status, 1)
        first = summary["servers"][0]
        self.assertEqual(first["failure"], {"phase": "wrong-key", "error": "worker_exit"})
        self.assertEqual([call[3] for call in calls if call[2] == "merovingian"], ["seed", "wrong-key"])
        self.assertEqual([call[3] for call in calls if call[2] == "synapse"], [phase[0] for phase in matrix_restart.PHASES])
        output = (self.run_path / "summary.json").read_text(encoding="utf-8")
        self.assertNotIn("SECRET", output)
        self.assertNotIn("access_token", output)

    def test_passed_report_with_failed_wrong_key_check_is_rejected(self):
        calls = []
        with patch.object(
            matrix_restart.subprocess,
            "run",
            side_effect=self.fake_worker(calls, false_check_phase="wrong-key"),
        ):
            summary, status = matrix_restart.run_harness(
                self.profiles_path, self.run_path, binary=self.binary
            )

        self.assertEqual(status, 1)
        self.assertEqual(
            summary["servers"][0]["failure"],
            {"phase": "wrong-key", "error": "report_invalid"},
        )
        self.assertEqual([call[3] for call in calls if call[2] == "merovingian"], ["seed", "wrong-key"])

    def test_reused_process_id_is_rejected(self):
        calls = []
        with patch.object(
            matrix_restart.subprocess,
            "run",
            side_effect=self.fake_worker(calls, duplicate_pid=True),
        ):
            summary, status = matrix_restart.run_harness(
                self.profiles_path, self.run_path, binary=self.binary
            )

        self.assertEqual(status, 1)
        self.assertEqual(
            summary["servers"][0]["failure"],
            {"phase": "wrong-key", "error": "pid_reused"},
        )

    def test_rejects_non_loopback_and_existing_run_directory(self):
        profiles = self.profiles()
        profiles[0]["base_url"] = "http://example.com:18080"
        self.profiles_path.write_text(json.dumps(profiles), encoding="utf-8")
        with self.assertRaises(matrix_restart.HarnessError):
            matrix_restart.run_harness(self.profiles_path, self.run_path, binary=self.binary)

        self.profiles_path.write_text(json.dumps(self.profiles()), encoding="utf-8")
        self.run_path.mkdir()
        with self.assertRaises(matrix_restart.HarnessError):
            matrix_restart.run_harness(self.profiles_path, self.run_path, binary=self.binary)


if __name__ == "__main__":
    unittest.main()
