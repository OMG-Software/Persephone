import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import tempfile
import threading
import unittest
from urllib.error import HTTPError
from urllib.request import ProxyHandler, Request, build_opener

from tools import matrix_capabilities


class _FakeHomeserver:
    def __init__(self):
        self.paths = []
        self.encodings = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, _format, *args):
                return

            def do_GET(self):
                owner.encodings.append(self.headers.get("Accept-Encoding"))
                owner.paths.append((self.command, self.path))
                if self.path == "/_matrix/client/versions":
                    body = json.dumps({
                        "versions": ["v1.19"],
                        "unstable_features": {
                            "org.matrix.simplified_msc3575": True,
                            "org.matrix.msc4186": True,
                        },
                    }).encode()
                else:
                    body = b'{"next_batch":"pos"}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                owner.paths.append((self.command, self.path))
                self.rfile.read(int(self.headers.get("Content-Length", "0")))
                body = b'{"next_batch":"pos"}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def origin(self):
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self):
        self.server.shutdown()
        self.thread.join(timeout=2)
        self.server.server_close()


def _request(url, method="GET", body=None, timeout=2):
    request = Request(url, data=body, method=method, headers={"Content-Type": "application/json"})
    return build_opener(ProxyHandler({})).open(request, timeout=timeout)


class MatrixCapabilitiesGatewayTests(unittest.TestCase):
    def setUp(self):
        self.upstream = _FakeHomeserver()

    def tearDown(self):
        self.upstream.close()

    def _gateway(self, scenario):
        gateway = matrix_capabilities.LoopbackGateway(self.upstream.origin, scenario)
        gateway.start()
        self.addCleanup(gateway.close)
        return gateway

    def test_capability_controls_are_distinct_and_native_paths_are_blocked(self):
        absent = self._gateway("absent")
        disabled = self._gateway("disabled")

        with _request(absent.origin + "/_matrix/client/versions") as response:
            absent_versions = json.load(response)
        with _request(disabled.origin + "/_matrix/client/versions") as response:
            disabled_versions = json.load(response)

        self.assertNotIn("org.matrix.simplified_msc3575", absent_versions["unstable_features"])
        self.assertNotIn("org.matrix.msc4186", absent_versions["unstable_features"])
        self.assertIs(disabled_versions["unstable_features"]["org.matrix.simplified_msc3575"], False)
        self.assertIs(disabled_versions["unstable_features"]["org.matrix.msc4186"], False)

        for gateway in (absent, disabled):
            with self.assertRaises(HTTPError) as caught:
                _request(gateway.origin + "/_matrix/client/unstable/org.matrix.msc4186/sync", "POST", b"{}")
            try:
                self.assertEqual(caught.exception.code, 404)
                payload = json.load(caught.exception)
                self.assertEqual(payload["errcode"], "M_UNRECOGNIZED")
                self.assertEqual(gateway.counts()["blocked_native"], 1)
            finally:
                caught.exception.close()
        self.assertEqual(
            [path for method, path in self.upstream.paths if method == "POST"],
            [],
            "blocked native requests must never reach the homeserver",
        )

    def test_advertised_native_and_classic_requests_forward(self):
        gateway = self._gateway("advertised")
        with _request(gateway.origin + "/_matrix/client/versions") as response:
            versions = json.load(response)
        self.assertIs(versions["unstable_features"]["org.matrix.simplified_msc3575"], True)
        with _request(gateway.origin + "/_matrix/client/v3/sync?timeout=0") as response:
            self.assertEqual(json.load(response)["next_batch"], "pos")
        with _request(
            gateway.origin + "/_matrix/client/unstable/org.matrix.msc4186/sync",
            "POST",
            b"{}",
        ) as response:
            self.assertEqual(json.load(response)["next_batch"], "pos")
        self.assertIn(("GET", "/_matrix/client/v3/sync?timeout=0"), self.upstream.paths)
        self.assertIn(("POST", "/_matrix/client/unstable/org.matrix.msc4186/sync"), self.upstream.paths)
        self.assertEqual(gateway.counts()["native_sync"], 1)
        self.assertEqual(gateway.counts()["classic_sync"], 1)

    def test_gateway_requests_identity_encoding_when_client_accepts_gzip(self):
        gateway = self._gateway("absent")
        request = Request(gateway.origin + "/_matrix/client/versions", headers={"Accept-Encoding": "gzip"})
        with build_opener(ProxyHandler({})).open(request, timeout=2) as response:
            versions = json.load(response)
        self.assertNotIn("org.matrix.simplified_msc3575", versions["unstable_features"])
        self.assertEqual(self.upstream.encodings, ["identity"])

    def test_discovery_failure_responses_are_controlled(self):
        for scenario, expected_status in (
            ("discovery-http-401", 401),
            ("discovery-http-403", 403),
            ("discovery-http-404", 404),
            ("discovery-http-429", 429),
            ("discovery-http-503", 503),
        ):
            with self.subTest(scenario=scenario):
                gateway = self._gateway(scenario)
                try:
                    with self.assertRaises(HTTPError) as caught:
                        _request(gateway.origin + "/_matrix/client/versions")
                    self.assertEqual(caught.exception.code, expected_status)
                    self.assertEqual(gateway.counts()["versions"], 1)
                    self.assertEqual(self.upstream.paths, [])
                    caught.exception.close()
                finally:
                    gateway.close()

    def test_wrong_type_discovery_keeps_a_valid_versions_object(self):
        gateway = self._gateway("discovery-wrong-type")
        with _request(gateway.origin + "/_matrix/client/versions") as response:
            payload = json.load(response)
        self.assertEqual(payload["versions"], ["v1.19"])
        self.assertEqual(payload["unstable_features"]["org.matrix.simplified_msc3575"], "true")
        self.assertEqual(gateway.counts()["versions"], 1)
        self.assertEqual(self.upstream.paths, [])

    def test_expected_counters_reject_classic_use_forward_failures_and_second_native_cycle(self):
        counts = {
            "versions": 1,
            "classic_sync": 0,
            "native_sync": 1,
            "blocked_native": 0,
            "upstream_forward_failures": 0,
        }
        self.assertTrue(matrix_capabilities._expected_counts("advertised", counts))
        self.assertFalse(matrix_capabilities._expected_counts("native-http-401", {**counts, "native_sync": 2}))
        self.assertFalse(matrix_capabilities._expected_counts("advertised", {**counts, "classic_sync": 1}))
        self.assertFalse(matrix_capabilities._expected_counts("absent", {**counts, "upstream_forward_failures": 1}))

    @staticmethod
    def _positive_report():
        return {
            "server": "synapse",
            "version": "1.163.0",
            "scenario": "discover-sync",
            "passed": True,
            "selected_mode": "sliding",
            "both_logouts_confirmed": True,
            "checks": {
                "mutual_sas_and_exact_bidirectional_decrypted_sync_receipts": True,
                "identity_keys_and_peer_verification_preserved": True,
                "native_rounds": 2,
                "classic_rounds": 0,
            },
        }

    def test_report_validation_rejects_wrong_selected_mode(self):
        with tempfile.TemporaryDirectory(dir=matrix_capabilities.LOCAL_ROOT) as temp:
            report_path = Path(temp) / "report.json"
            report = self._positive_report()
            report["selected_mode"] = "classic"
            report["checks"]["native_rounds"] = 0
            report["checks"]["classic_rounds"] = 2
            report_path.write_text(json.dumps(report), encoding="utf-8")
            with self.assertRaises(matrix_capabilities.HarnessError):
                matrix_capabilities._read_worker_report(
                    report_path,
                    server="synapse",
                    version="1.163.0",
                    command_flag="discover-sync",
                    expected_mode="sliding",
                )

    def test_report_validation_rejects_unknown_top_level_secret_field(self):
        with tempfile.TemporaryDirectory(dir=matrix_capabilities.LOCAL_ROOT) as temp:
            report_path = Path(temp) / "report.json"
            report = self._positive_report()
            report["access_token"] = "synthetic-secret"
            report_path.write_text(json.dumps(report), encoding="utf-8")
            with self.assertRaises(matrix_capabilities.HarnessError):
                matrix_capabilities._read_worker_report(
                    report_path,
                    server="synapse",
                    version="1.163.0",
                    command_flag="discover-sync",
                    expected_mode="sliding",
                )


if __name__ == "__main__":
    unittest.main()
