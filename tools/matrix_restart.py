"""Run the encrypted Matrix-store worker through separate OS processes.

All profiles, stores, phase reports, and the summary are kept under the ignored
repository ``.local`` directory. Encryption keys travel only through child
stdin and are never written to disk or included in a command line.
"""

from __future__ import annotations

import argparse
import ipaddress
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
from typing import Any
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[1]
LOCAL_ROOT = (ROOT / ".local").resolve()
if os.name == "nt":
    DEFAULT_BINARY = LOCAL_ROOT / "build" / "rust" / "debug" / "verified_recipients.exe"
else:
    DEFAULT_BINARY = LOCAL_ROOT / "build" / "rust" / "debug" / "verified_recipients"

PHASES = (
    ("seed", "--restart-seed", "seed"),
    ("wrong-key", "--restart-wrong-key", "wrong-key"),
    ("missing-key", "--restart-missing-key", "missing-key"),
    ("offline", "--restart-offline", "offline"),
    ("resume", "--restart-resume", "resume"),
)
EXPECTED_SERVERS = {"merovingian", "synapse"}
REPORT_KEYS = {"server", "version", "phase", "pid", "passed", "checks"}
REQUIRED_CHECKS: dict[str, dict[str, tuple[type, Any]]] = {
    "seed": {
        "mutual_sas_and_bidirectional_native_e2ee": (bool, True),
        "encrypted_session_checkpoints_read_back": (bool, True),
        "no_token_key_or_message_markers_in_store_artifacts": (bool, True),
        "native_rounds": (int, 1),
        "classic_rounds": (int, 0),
    },
    "wrong-key": {"both_wrong_keys_rejected": (bool, True)},
    "missing-key": {"missing_keys_rejected_without_plaintext_fallback": (bool, True)},
    "offline": {
        "same_users_devices_keys_and_mutual_verification_before_sync": (bool, True),
        "persisted_room_keys_decrypt_both_old_exact_events": (bool, True),
        "password_login_and_sas_repeated": (bool, False),
        "closed_origin_no_sync_or_history_requests": (bool, True),
    },
    "resume": {
        "same_users_devices_keys_and_mutual_verification_before_sync": (bool, True),
        "persisted_room_keys_decrypt_both_old_exact_events": (bool, True),
        "password_login_and_sas_repeated": (bool, False),
        "new_bidirectional_native_decrypted_sync_receipts": (bool, True),
        "same_security_state_after_new_messages": (bool, True),
        "no_token_key_or_message_markers_in_store_artifacts": (bool, True),
        "both_logouts_confirmed": (bool, True),
        "native_rounds": (int, 1),
        "classic_rounds": (int, 0),
    },
}
OPTIONAL_CHECKS: dict[str, dict[str, type]] = {
    "seed": {"scanned_files": int},
    "wrong-key": {},
    "missing-key": {},
    "offline": {},
    "resume": {"scanned_files": int},
}


class HarnessError(Exception):
    """An input or destination error safe to show without profile contents."""


def _inside_local(path: Path, *, must_exist: bool) -> Path:
    try:
        resolved = path.resolve(strict=must_exist)
    except (OSError, RuntimeError) as error:
        raise HarnessError("path_invalid") from error
    if not resolved.is_relative_to(LOCAL_ROOT):
        raise HarnessError("path_outside_local")
    return resolved


def _validate_origin(value: Any) -> bool:
    if not isinstance(value, str):
        return False
    try:
        parsed = urlsplit(value)
        port = parsed.port
        host = parsed.hostname
        ip = ipaddress.ip_address(host) if host is not None else None
    except (ValueError, TypeError):
        return False
    return (
        parsed.scheme == "http"
        and port is not None
        and 1 <= port <= 65535
        and parsed.path in ("", "/")
        and parsed.query == ""
        and parsed.fragment == ""
        and parsed.username is None
        and parsed.password is None
        and ip is not None
        and ip.is_loopback
    )


def _validate_profiles(value: Any) -> list[dict[str, Any]]:
    if not isinstance(value, list) or len(value) != 2:
        raise HarnessError("profiles_invalid")

    profiles: list[dict[str, Any]] = []
    names: set[str] = set()
    for raw in value:
        if not isinstance(raw, dict):
            raise HarnessError("profiles_invalid")
        name = raw.get("name")
        version = raw.get("version")
        username = raw.get("username")
        password = raw.get("password")
        recipient = raw.get("recipient")
        if (
            name not in EXPECTED_SERVERS
            or name in names
            or not isinstance(version, str)
            or not version.strip()
            or not _validate_origin(raw.get("base_url"))
            or username != "p0_alice"
            or not isinstance(password, str)
            or not password
            or not isinstance(recipient, dict)
            or recipient.get("username") != "p0_bob"
            or not isinstance(recipient.get("password"), str)
            or not recipient["password"]
            or password == recipient["password"]
        ):
            raise HarnessError("profiles_invalid")
        names.add(name)

        # Build only the fields consumed by the worker. This prevents unrelated
        # input fields from being copied into the run directory.
        profiles.append(
            {
                "name": name,
                "version": version,
                "base_url": raw["base_url"],
                "username": username,
                "password": password,
                "recipient": {
                    "username": recipient["username"],
                    "password": recipient["password"],
                },
            }
        )

    if names != EXPECTED_SERVERS:
        raise HarnessError("profiles_invalid")
    return sorted(profiles, key=lambda profile: profile["name"])


def _safe_report(path: Path, *, server: str, version: str, phase: str) -> dict[str, Any]:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise HarnessError("report_invalid") from error
    if not isinstance(raw, dict):
        raise HarnessError("report_invalid")
    if set(raw) - REPORT_KEYS:
        # Extra fields may contain session material. They are discarded below,
        # but their presence is unexpected for this protocol.
        raw = {key: raw[key] for key in REPORT_KEYS if key in raw}
    checks = raw.get("checks")
    if not isinstance(checks, dict) or any(
        type(check) not in (bool, int) for check in checks.values()
    ):
        raise HarnessError("report_invalid")
    required_check_keys = set(REQUIRED_CHECKS[phase])
    optional_check_keys = set(OPTIONAL_CHECKS[phase])
    if not required_check_keys.issubset(checks) or not set(checks).issubset(
        required_check_keys | optional_check_keys
    ):
        raise HarnessError("report_invalid")
    pid = raw.get("pid")
    if (
        raw.get("server") != server
        or raw.get("version") != version
        or raw.get("phase") != phase
        or type(pid) is not int
        or pid <= 0
        or raw.get("passed") is not True
    ):
        raise HarnessError("report_invalid")
    for key, (expected_type, expected_value) in REQUIRED_CHECKS[phase].items():
        actual = checks.get(key)
        if type(actual) is not expected_type:
            raise HarnessError("report_invalid")
        if expected_type is bool and actual is not expected_value:
            raise HarnessError("report_invalid")
        if expected_type is int:
            if (expected_value == 1 and actual <= 0) or (
                expected_value == 0 and actual != 0
            ):
                raise HarnessError("report_invalid")
    for key, expected_type in OPTIONAL_CHECKS[phase].items():
        if key in checks and type(checks[key]) is not expected_type:
            raise HarnessError("report_invalid")
        if key in checks and checks[key] <= 0:
            raise HarnessError("report_invalid")

    # This projection is the only report data that may reach the summary.
    return {
        "server": server,
        "version": version,
        "phase": phase,
        "pid": pid,
        "passed": True,
        "checks": dict(checks),
    }


def _write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def run_harness(profiles_path: Path, run_path: Path, *, binary: Path = DEFAULT_BINARY) -> tuple[dict[str, Any], int]:
    """Run all process phases, returning a sanitized summary and exit code."""
    profiles_path = _inside_local(profiles_path, must_exist=True)
    if not profiles_path.is_file():
        raise HarnessError("profiles_invalid")
    run_path = _inside_local(run_path, must_exist=False)
    if run_path == LOCAL_ROOT or run_path.exists():
        raise HarnessError("run_directory_exists")
    binary = _inside_local(binary, must_exist=False)

    try:
        profiles = _validate_profiles(json.loads(profiles_path.read_text(encoding="utf-8")))
    except HarnessError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise HarnessError("profiles_invalid") from error

    run_path.mkdir(parents=True, exist_ok=False)
    server_results: list[dict[str, Any]] = []
    seen_pids: set[int] = set()
    all_passed = True

    for profile in profiles:
        server = profile["name"]
        version = profile["version"]
        server_dir = run_path / server
        server_dir.mkdir()
        seed_profile_path = server_dir / "seed-profile.json"
        resume_profile_path = server_dir / "resume-profile.json"
        store_root = server_dir / "store"

        _write_json(seed_profile_path, [profile])
        _write_json(
            resume_profile_path,
            [{"name": server, "version": version, "base_url": profile["base_url"]}],
        )

        key_bundle = secrets.token_bytes(64)
        server_phases: list[dict[str, Any]] = []
        failure: dict[str, str] | None = None

        for phase, flag, input_kind in PHASES:
            phase_report_path = server_dir / f"{phase}.json"
            if input_kind in ("seed", "offline", "resume"):
                stdin_bytes = key_bundle
            elif input_kind == "wrong-key":
                stdin_bytes = secrets.token_bytes(64)
            else:
                stdin_bytes = b""

            profile_path = seed_profile_path if phase == "seed" else resume_profile_path
            command = [
                str(binary),
                str(profile_path),
                str(phase_report_path),
                flag,
                str(store_root),
            ]
            try:
                completed = subprocess.run(
                    command,
                    cwd=ROOT,
                    input=stdin_bytes,
                    capture_output=True,
                    timeout=180,
                    check=False,
                )
            except subprocess.TimeoutExpired:
                phase_report_path.unlink(missing_ok=True)
                failure = {"phase": phase, "error": "worker_timeout"}
                print(f"{server}: {phase} FAIL", flush=True)
                break
            except OSError:
                phase_report_path.unlink(missing_ok=True)
                failure = {"phase": phase, "error": "worker_unavailable"}
                print(f"{server}: {phase} FAIL", flush=True)
                break

            if completed.returncode != 0:
                phase_report_path.unlink(missing_ok=True)
                failure = {"phase": phase, "error": "worker_exit"}
                print(f"{server}: {phase} FAIL", flush=True)
                break

            try:
                report = _safe_report(
                    phase_report_path, server=server, version=version, phase=phase
                )
            except HarnessError:
                phase_report_path.unlink(missing_ok=True)
                failure = {"phase": phase, "error": "report_invalid"}
                print(f"{server}: {phase} FAIL", flush=True)
                break
            if report["pid"] in seen_pids:
                phase_report_path.unlink(missing_ok=True)
                failure = {"phase": phase, "error": "pid_reused"}
                print(f"{server}: {phase} FAIL", flush=True)
                break
            seen_pids.add(report["pid"])
            # Replace the worker's file with the validated projection so an
            # accidental extra field can never persist in the run artifacts.
            _write_json(phase_report_path, report)
            server_phases.append(report)
            print(f"{server}: {phase} PASS", flush=True)

        result: dict[str, Any] = {"server": server, "phases": server_phases}
        if failure is not None:
            result["failure"] = failure
            all_passed = False
        else:
            result["passed"] = True
        server_results.append(result)

    summary = {"passed": all_passed, "servers": server_results}
    _write_json(run_path / "summary.json", summary)
    return summary, 0 if all_passed else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profiles", type=Path, help="ignored disposable two-server profile JSON")
    parser.add_argument("run_directory", type=Path, help="new output directory under .local")
    args = parser.parse_args(argv)
    try:
        _, status = run_harness(args.profiles, args.run_directory)
    except HarnessError as error:
        print(f"matrix restart harness failed: {error}", file=sys.stderr)
        return 2
    except OSError:
        print("matrix restart harness failed: output_error", file=sys.stderr)
        return 2
    if status:
        print("matrix restart harness failed; see sanitized summary.json", file=sys.stderr)
    return status


if __name__ == "__main__":
    raise SystemExit(main())
