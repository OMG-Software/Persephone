"""Exercise Matrix sync capability handling through a disposable loopback gateway.

The gateway forwards ordinary client API requests to an explicitly loopback
homeserver, with tightly scoped control over /versions and native sync paths.
It does not change the homeserver's own advertised features or configuration.
All generated profiles and reports belong under the ignored repository .local.
"""

from __future__ import annotations

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import ipaddress
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener


ROOT = Path(__file__).resolve().parents[1]
LOCAL_ROOT = (ROOT / ".local").resolve()
DEFAULT_BINARY = LOCAL_ROOT / "build" / "rust" / "debug" / (
    "verified_recipients.exe" if os.name == "nt" else "verified_recipients"
)
MAX_BODY = 2 * 1024 * 1024
SCENARIOS: tuple[tuple[str, str, str | None], ...] = (
    ("advertised", "discover-sync", "sliding"),
    ("absent", "discover-sync", "classic"),
    ("disabled", "discover-sync", "classic"),
    ("discovery-http-401", "expect-discovery-error", None),
    ("discovery-http-403", "expect-discovery-error", None),
    ("discovery-http-404", "expect-discovery-error", None),
    ("discovery-http-429", "expect-discovery-error", None),
    ("discovery-http-503", "expect-discovery-error", None),
    ("discovery-malformed", "expect-discovery-error", None),
    ("discovery-wrong-type", "expect-discovery-error", None),
    ("discovery-timeout", "expect-discovery-error", None),
    ("native-http-401", "expect-native-error", "sliding"),
    ("native-http-404", "expect-native-error", "sliding"),
    ("native-http-503", "expect-native-error", "sliding"),
    ("native-unknown-pos", "expect-native-error", "sliding"),
)
SERVER_NAMES = {"merovingian", "synapse"}
DISCOVERY_CHECKS = {"discovery_failed_before_engine_creation": (bool, True)}
NATIVE_ERROR_CHECKS = {
    "sync_failed_without_changing_mode": (bool, True),
    "failed_engine_refused_second_cycle": (bool, True),
    "native_rounds": (int, 0),
    "classic_rounds": (int, 0),
}
POSITIVE_CHECKS = {
    "mutual_sas_and_exact_bidirectional_decrypted_sync_receipts": bool,
    "identity_keys_and_peer_verification_preserved": bool,
    "native_rounds": int,
    "classic_rounds": int,
}


class HarnessError(Exception):
    """A failure whose message is safe to persist or print."""


def _inside_local(path: Path, *, must_exist: bool) -> Path:
    try:
        resolved = path.resolve(strict=must_exist)
    except (OSError, RuntimeError) as error:
        raise HarnessError("path_invalid") from error
    if not resolved.is_relative_to(LOCAL_ROOT):
        raise HarnessError("path_outside_local")
    return resolved


def _valid_origin(value: Any) -> bool:
    if not isinstance(value, str):
        return False
    try:
        parsed = urlsplit(value)
        host = parsed.hostname
        port = parsed.port
        ip = ipaddress.ip_address(host) if host is not None else None
    except (TypeError, ValueError):
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


def _load_profiles(path: Path) -> list[dict[str, str]]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise HarnessError("profiles_invalid") from error
    if not isinstance(value, list) or len(value) != 2:
        raise HarnessError("profiles_invalid")
    profiles: list[dict[str, str]] = []
    seen: set[str] = set()
    for profile in value:
        if not isinstance(profile, dict):
            raise HarnessError("profiles_invalid")
        name = profile.get("name")
        version = profile.get("version")
        recipient = profile.get("recipient")
        if (
            name not in SERVER_NAMES
            or name in seen
            or not isinstance(version, str)
            or not version.strip()
            or not _valid_origin(profile.get("base_url"))
            or profile.get("username") != "p0_alice"
            or not isinstance(profile.get("password"), str)
            or not profile["password"]
            or not isinstance(recipient, dict)
            or recipient.get("username") != "p0_bob"
            or not isinstance(recipient.get("password"), str)
            or not recipient["password"]
            or profile["password"] == recipient["password"]
        ):
            raise HarnessError("profiles_invalid")
        seen.add(name)
        profiles.append({
            "name": name,
            "version": version,
            "base_url": profile["base_url"].rstrip("/"),
            "username": "p0_alice",
            "password": profile["password"],
            "recipient_username": "p0_bob",
            "recipient_password": recipient["password"],
        })
    if seen != SERVER_NAMES:
        raise HarnessError("profiles_invalid")
    return sorted(profiles, key=lambda profile: profile["name"])


def _json_bytes(status: int, body: Any) -> tuple[int, bytes, str]:
    return status, json.dumps(body, separators=(",", ":")).encode(), "application/json"


def _native_sync_path(path: str) -> bool:
    parsed = urlsplit(path)
    if parsed.query:
        path = parsed.path
    return (
        path == "/_matrix/client/v4/sync"
        or path.endswith("/org.matrix.msc4186/sync")
        or path.endswith("/org.matrix.msc3575/sync")
        or path.endswith("/org.matrix.simplified_msc3575/sync")
    )


class _NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class LoopbackGateway:
    """A no-log HTTP origin proxy with scenario-scoped response controls."""

    def __init__(self, upstream: str, scenario: str):
        if not _valid_origin(upstream):
            raise HarnessError("upstream_origin_invalid")
        self.upstream = upstream.rstrip("/")
        self.scenario = scenario
        self._counts = {
            "versions": 0,
            "classic_sync": 0,
            "native_sync": 0,
            "blocked_native": 0,
            "upstream_forward_failures": 0,
        }
        self._lock = threading.Lock()
        self._request_threads: set[threading.Thread] = set()
        self._request_threads_lock = threading.Lock()
        self._closed = False
        gateway = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def setup(self):
                super().setup()
                self.connection.settimeout(8)
                with gateway._request_threads_lock:
                    gateway._request_threads.add(threading.current_thread())

            def finish(self):
                try:
                    super().finish()
                finally:
                    with gateway._request_threads_lock:
                        gateway._request_threads.discard(threading.current_thread())

            def log_message(self, _format: str, *args: Any) -> None:
                # Default logging includes URL paths and can expose tokens.
                return

            def do_GET(self):
                self._handle()

            def do_POST(self):
                self._handle()

            def do_PUT(self):
                self._handle()

            def do_DELETE(self):
                self._handle()

            def _response(self, status: int, body: bytes, content_type: str):
                try:
                    self.send_response(status)
                    self.send_header("Content-Type", content_type)
                    self.send_header("Content-Length", str(len(body)))
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.wfile.write(body)
                except (ConnectionError, TimeoutError):
                    pass

            def _handle(self):
                path = self.path
                if not path.startswith("/_matrix/client/") or urlsplit(path).scheme:
                    self._response(400, b'{"errcode":"M_UNKNOWN","error":"Request target refused"}', "application/json")
                    return
                if self.headers.get("Transfer-Encoding"):
                    self._response(400, b'{"errcode":"M_UNKNOWN","error":"Request framing refused"}', "application/json")
                    return
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    if length < 0 or length > MAX_BODY:
                        raise ValueError
                except ValueError:
                    self._response(413, b'{"errcode":"M_TOO_LARGE","error":"Request body too large"}', "application/json")
                    return
                try:
                    body = self.rfile.read(length) if length else None
                except OSError:
                    return
                if body is not None and len(body) != length:
                    self._response(400, b'{"errcode":"M_UNKNOWN","error":"Request body incomplete"}', "application/json")
                    return

                is_versions = self.command == "GET" and path == "/_matrix/client/versions"
                is_native = self.command == "POST" and _native_sync_path(path)
                is_classic = self.command == "GET" and path.startswith("/_matrix/client/") and path.split("?", 1)[0].endswith("/sync")
                with gateway._lock:
                    if is_versions:
                        gateway._counts["versions"] += 1
                    if is_native:
                        gateway._counts["native_sync"] += 1
                        if gateway.scenario in ("absent", "disabled"):
                            gateway._counts["blocked_native"] += 1
                        else:
                            pass
                    if is_classic:
                        gateway._counts["classic_sync"] += 1

                controlled = gateway._controlled(self.command, path, is_versions, is_native)
                if controlled is not None:
                    status, payload, content_type, delay = controlled
                    if delay:
                        time.sleep(delay)
                    self._response(status, payload, content_type)
                    return

                try:
                    status, payload, content_type, response_headers = gateway._forward(
                        self.command, path, body, self.headers
                    )
                except (OSError, URLError, HTTPError, TimeoutError, ValueError):
                    with gateway._lock:
                        gateway._counts["upstream_forward_failures"] += 1
                    self._response(502, b'{"errcode":"M_UNKNOWN","error":"Upstream unavailable"}', "application/json")
                    return

                if is_versions and gateway.scenario in ("absent", "disabled"):
                    try:
                        version_json = json.loads(payload)
                        if not isinstance(version_json, dict):
                            raise ValueError
                        features = version_json.get("unstable_features")
                        if features is None:
                            features = {}
                        elif not isinstance(features, dict):
                            raise ValueError
                        if gateway.scenario == "absent":
                            features.pop("org.matrix.simplified_msc3575", None)
                            features.pop("org.matrix.msc4186", None)
                        else:
                            features["org.matrix.simplified_msc3575"] = False
                            features["org.matrix.msc4186"] = False
                        version_json["unstable_features"] = features
                        payload = json.dumps(version_json, separators=(",", ":")).encode()
                        content_type = "application/json"
                    except (ValueError, TypeError, json.JSONDecodeError):
                        self._response(502, b'{"errcode":"M_UNKNOWN","error":"Upstream discovery malformed"}', "application/json")
                        return
                self._response(status, payload, content_type)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.server.timeout = 0.25
        self._thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self._started = False

    @property
    def origin(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def start(self) -> None:
        self._thread.start()
        self._started = True

    def close(self) -> None:
        if self._started and not self._closed:
            self.server.shutdown()
            self._thread.join(timeout=3)
            with self._request_threads_lock:
                active = tuple(self._request_threads)
            for request_thread in active:
                request_thread.join(timeout=9)
            if any(request_thread.is_alive() for request_thread in active):
                self.server.server_close()
                self._closed = True
                raise HarnessError("gateway_shutdown_timeout")
        self.server.server_close()
        self._closed = True

    def counts(self) -> dict[str, int]:
        with self._lock:
            return dict(self._counts)

    def _controlled(self, method: str, path: str, is_versions: bool, is_native: bool):
        scenario = self.scenario
        if scenario in ("absent", "disabled") and is_native:
            return _json_bytes(404, {"errcode": "M_UNRECOGNIZED", "error": "Unrecognized request"}) + (0,)
        if is_versions:
            # Each status with the Matrix v1.19 error code it comes with.
            status_by_scenario = {
                "discovery-http-401": (401, "M_UNKNOWN_TOKEN"),
                "discovery-http-403": (403, "M_FORBIDDEN"),
                "discovery-http-404": (404, "M_UNRECOGNIZED"),
                "discovery-http-429": (429, "M_LIMIT_EXCEEDED"),
                "discovery-http-503": (503, "M_UNKNOWN"),
            }
            if scenario in status_by_scenario:
                status, errcode = status_by_scenario[scenario]
                return _json_bytes(status, {"errcode": errcode, "error": "Controlled discovery response"}) + (0,)
            if scenario == "discovery-malformed":
                return 200, b"{not-json", "application/json", 0
            if scenario == "discovery-wrong-type":
                return (
                    200,
                    b'{"versions":["v1.19"],"unstable_features":{"org.matrix.simplified_msc3575":"true"}}',
                    "application/json",
                    0,
                )
            if scenario == "discovery-timeout":
                return 200, b"{}", "application/json", 4.0
        if is_native and scenario.startswith("native-"):
            # M_UNKNOWN_POS is Sliding Sync's own code (MSC4186), not Matrix v1.19's.
            native = {
                "native-http-401": (401, "M_UNKNOWN_TOKEN", "Controlled native sync response"),
                "native-http-404": (404, "M_UNRECOGNIZED", "Controlled native sync response"),
                "native-http-503": (503, "M_UNKNOWN", "Controlled native sync response"),
                "native-unknown-pos": (400, "M_UNKNOWN_POS", "Unknown position"),
            }.get(scenario)
            if native is not None:
                status, errcode, error = native
                return _json_bytes(status, {"errcode": errcode, "error": error}) + (0,)
        return None

    def _forward(self, method, path, body, headers):
        blocked = {
            "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
            "te", "trailer", "transfer-encoding", "upgrade", "host", "content-length",
            "accept-encoding",
        }
        connection_tokens = {
            token.strip().lower()
            for token in headers.get("Connection", "").split(",")
            if token.strip()
        }
        blocked.update(connection_tokens)
        outgoing = {key: value for key, value in headers.items() if key.lower() not in blocked}
        request = Request(self.upstream + path, data=body, headers=outgoing, method=method)
        opener = build_opener(ProxyHandler({}), _NoRedirect())
        try:
            with opener.open(request, timeout=8) as response:
                payload = response.read(MAX_BODY + 1)
                status = response.status
                response_headers = response.headers
        except HTTPError as error:
            try:
                payload = error.read(MAX_BODY + 1)
                status = error.code
                response_headers = error.headers
            finally:
                error.close()
        if len(payload) > MAX_BODY:
            raise ValueError("upstream response too large")
        if response_headers.get("Content-Encoding", "identity").lower() != "identity":
            raise ValueError("unexpected upstream content encoding")
        content_type = response_headers.get("Content-Type", "application/octet-stream")
        return status, payload, content_type, response_headers


def _expected_counts(scenario: str, counts: dict[str, int]) -> bool:
    if counts["upstream_forward_failures"] != 0:
        return False
    if scenario in ("advertised",):
        return (
            counts["versions"] > 0
            and counts["native_sync"] > 0
            and counts["classic_sync"] == 0
            and counts["blocked_native"] == 0
        )
    if scenario in ("absent", "disabled"):
        return counts["versions"] > 0 and counts["classic_sync"] > 0 and counts["native_sync"] == 0
    if scenario.startswith("discovery-"):
        return counts["versions"] > 0 and counts["classic_sync"] == 0 and counts["native_sync"] == 0
    if scenario.startswith("native-"):
        return (
            counts["versions"] > 0
            and counts["native_sync"] == 1
            and counts["classic_sync"] == 0
        )
    return False


def _read_worker_report(
    path: Path,
    *,
    server: str,
    version: str,
    command_flag: str,
    expected_mode: str | None,
) -> dict[str, Any]:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise HarnessError("report_invalid") from error
    if not isinstance(raw, dict):
        raise HarnessError("report_invalid")
    if (
        raw.get("server") != server
        or raw.get("version") != version
        or raw.get("scenario") != command_flag
    ):
        raise HarnessError("report_invalid")
    if raw.get("passed") is not True:
        raise HarnessError("report_invalid")
    checks = raw.get("checks")
    if not isinstance(checks, dict) or any(type(value) not in (bool, int) for value in checks.values()):
        raise HarnessError("report_invalid")
    selected_mode = raw.get("selected_mode")
    if selected_mode not in ("sliding", "classic", None):
        raise HarnessError("report_invalid")
    allowed_keys = {
        "server", "version", "scenario", "passed", "selected_mode", "checks",
        "both_logouts_confirmed",
    }
    if set(raw) != allowed_keys:
        raise HarnessError("report_invalid")
    if selected_mode != expected_mode or type(raw["both_logouts_confirmed"]) is not bool:
        raise HarnessError("report_invalid")

    if command_flag == "discover-sync":
        if set(checks) != set(POSITIVE_CHECKS):
            raise HarnessError("report_invalid")
        if any(type(checks[key]) is not value_type for key, value_type in POSITIVE_CHECKS.items()):
            raise HarnessError("report_invalid")
        if (
            checks["mutual_sas_and_exact_bidirectional_decrypted_sync_receipts"] is not True
            or checks["identity_keys_and_peer_verification_preserved"] is not True
            or checks["native_rounds"] < 0
            or checks["classic_rounds"] < 0
            or (expected_mode == "sliding" and (checks["native_rounds"] <= 0 or checks["classic_rounds"] != 0))
            or (expected_mode == "classic" and (checks["classic_rounds"] <= 0 or checks["native_rounds"] != 0))
            or raw["both_logouts_confirmed"] is not True
        ):
            raise HarnessError("report_invalid")
    elif command_flag == "expect-discovery-error":
        if (
            set(checks) != set(DISCOVERY_CHECKS)
            or checks.get("discovery_failed_before_engine_creation") is not True
            or selected_mode is not None
            or raw["both_logouts_confirmed"] is not False
        ):
            raise HarnessError("report_invalid")
    elif command_flag == "expect-native-error":
        if set(checks) != set(NATIVE_ERROR_CHECKS):
            raise HarnessError("report_invalid")
        for key, (expected_type, expected_value) in NATIVE_ERROR_CHECKS.items():
            if type(checks[key]) is not expected_type or checks[key] != expected_value:
                raise HarnessError("report_invalid")
        if raw["both_logouts_confirmed"] is not True:
            raise HarnessError("report_invalid")
    else:
        raise HarnessError("report_invalid")
    return {
        "server": server,
        "version": version,
        "scenario": command_flag,
        "passed": True,
        "selected_mode": selected_mode,
        "both_logouts_confirmed": raw["both_logouts_confirmed"],
        "checks": dict(checks),
    }


def _write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def run_harness(profiles_path: Path, run_path: Path, *, binary: Path = DEFAULT_BINARY) -> tuple[dict[str, Any], int]:
    profiles_path = _inside_local(profiles_path, must_exist=True)
    if not profiles_path.is_file():
        raise HarnessError("profiles_invalid")
    run_path = _inside_local(run_path, must_exist=False)
    if run_path == LOCAL_ROOT or run_path.exists():
        raise HarnessError("run_directory_exists")
    binary = _inside_local(binary, must_exist=False)
    profiles = _load_profiles(profiles_path)
    run_path.mkdir(parents=True, exist_ok=False)

    results = []
    all_passed = True
    for profile in profiles:
        server = profile["name"]
        server_result = {"server": server, "version": profile["version"], "scenarios": []}
        server_dir = run_path / server
        server_dir.mkdir()
        for scenario, command_flag, expected_mode in SCENARIOS:
            gateway = LoopbackGateway(profile["base_url"], scenario)
            gateway.start()
            scenario_dir = server_dir / scenario
            scenario_dir.mkdir()
            generated_profile = [{
                "name": server,
                "version": profile["version"],
                "base_url": gateway.origin,
                "username": profile["username"],
                "password": profile["password"],
                "recipient": {
                    "username": profile["recipient_username"],
                    "password": profile["recipient_password"],
                },
            }]
            profile_path = scenario_dir / "gateway-profile.json"
            report_path = scenario_dir / "report.json"
            _write_json(profile_path, generated_profile)
            stdout_category = "worker_exit"
            try:
                try:
                    completed = subprocess.run(
                        [str(binary), str(profile_path), str(report_path), f"--{command_flag}"],
                        capture_output=True,
                        timeout=180,
                        check=False,
                    )
                except subprocess.TimeoutExpired:
                    raise HarnessError("worker_timeout") from None
                except OSError:
                    raise HarnessError("worker_start_failed") from None
                if completed.returncode != 0:
                    raise HarnessError(stdout_category)
                report = _read_worker_report(
                    report_path,
                    server=server,
                    version=profile["version"],
                    command_flag=command_flag,
                    expected_mode=expected_mode,
                )
                # Drain request handlers before inspecting the final counters.
                gateway.close()
                counts = gateway.counts()
                if not _expected_counts(scenario, counts):
                    raise HarnessError("gateway_counts_invalid")
                scenario_result = {**report, "gateway_scenario": scenario, "gateway": counts}
                server_result["scenarios"].append(scenario_result)
                print(f"{server}: {scenario} PASS", flush=True)
            except HarnessError as error:
                all_passed = False
                server_result["failure"] = {"scenario": scenario, "error": str(error)}
                server_result["scenarios"].append({
                    "server": server,
                    "version": profile["version"],
                    "scenario": command_flag,
                    "gateway_scenario": scenario,
                    "passed": False,
                    "error": str(error),
                    "gateway": gateway.counts(),
                })
                print(f"{server}: {scenario} FAIL ({error})", flush=True)
                break
            finally:
                gateway.close()
        results.append(server_result)
    summary = {"passed": all_passed, "servers": results}
    _write_json(run_path / "summary.json", summary)
    return summary, 0 if all_passed else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profiles", type=Path)
    parser.add_argument("run_directory", type=Path)
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    args = parser.parse_args(argv)
    try:
        _, status = run_harness(args.profiles, args.run_directory, binary=args.binary)
        return status
    except HarnessError as error:
        print(f"matrix_capabilities: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
