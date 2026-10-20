"""Disposable loopback-only homeservers. Never reads a production config.

Run under Linux/WSL with --merovingian-binary and --synapse-python.
Keeps servers alive until interrupted, or runs --command with P0_PROFILE set.
All server state is a fresh TemporaryDirectory, removed on normal teardown.
"""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())


def request(base, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(base + path, data=data,
                                 headers={"Content-Type": "application/json"})
    with OPENER.open(req, timeout=5) as response:
        payload = response.read(1024 * 1024 + 1)
        if len(payload) > 1024 * 1024:
            raise ValueError("Synthetic server response exceeded 1 MiB")
        return json.loads(payload)


def redact_secrets(payload, *secrets_to_redact):
    for secret in secrets_to_redact:
        if secret:
            payload = payload.replace(secret, "[REDACTED]")
            payload = payload.replace(json.dumps(secret)[1:-1], "[REDACTED]")
    return payload


def register_account(base, name, username, password, registration_auth, output):
    """Register one account with its own UIA flow and redact retained errors."""
    registration_body = {
        "username": username, "password": password,
        "auth": dict(registration_auth), "inhibit_login": True,
    }
    try:
        request(base, "/_matrix/client/v3/register", registration_body)
    except urllib.error.HTTPError as error:
        if error.code != 401:
            payload = error.read(4096).decode(errors="replace")
            error.close()
            payload = redact_secrets(
                payload, password, registration_auth.get("token", ""))
            (output / f"{name}-registration-error.txt").write_text(payload)
            raise RuntimeError(
                f"Synthetic registration returned HTTP {error.code}; "
                "see local redacted error file") from None
        try:
            challenge = json.load(error)
        finally:
            error.close()
        # This body and session are local to this account. The next account
        # starts from an untouched copy of registration_auth.
        registration_body["auth"]["session"] = challenge["session"]
        request(base, "/_matrix/client/v3/register", registration_body)


def wait_ready(base, process):
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"Server exited with {process.returncode}; inspect local log")
        try:
            return request(base, "/_matrix/client/versions")
        except (OSError, ValueError):
            time.sleep(0.2)
    raise TimeoutError("Disposable server readiness deadline exceeded")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--merovingian-binary", type=Path, required=True)
    parser.add_argument("--synapse-python", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True,
                        help="Ignored local output directory; contains test credentials")
    parser.add_argument("--command", nargs=argparse.REMAINDER)
    parser.add_argument("--server", choices=("merovingian", "synapse", "both"), default="both")
    parser.add_argument("--synapse-classic-only", action="store_true",
                        help="Disable Synapse native Sliding Sync in its disposable config")
    args = parser.parse_args()
    if args.synapse_classic_only and args.server == "merovingian":
        parser.error("--synapse-classic-only requires a Synapse instance")
    if sys.platform != "linux":
        raise RuntimeError("Run disposable homeservers inside Linux/WSL")
    process_context = {"uid": os.getuid()}
    for line in Path("/proc/self/status").read_text().splitlines():
        key, _, value = line.partition(":")
        if key in ("CapEff", "CapBnd", "NoNewPrivs"):
            process_context[key] = value.strip()
    if args.server in ("merovingian", "both"):
        if (process_context["uid"] == 0
                or int(process_context["CapEff"], 16) != 0
                or int(process_context["CapBnd"], 16) != 0
                or process_context["NoNewPrivs"] != "1"):
            raise RuntimeError("Merovingian probe requires a non-root WSL/Linux context with all capabilities pre-dropped and no_new_privs set")
    ignored_root = Path(__file__).resolve().parents[2] / ".local"
    args.output = args.output.resolve()
    if not args.output.is_relative_to(ignored_root.resolve()):
        raise ValueError("Output must be inside the repository's ignored .local directory")
    args.output.mkdir(parents=True, exist_ok=True)
    # Deliberately require empty output: do not overwrite a previous test profile.
    if any(args.output.iterdir()):
        raise ValueError("Output directory must be empty")
    (args.output / "runner.pid").write_text(str(os.getpid()))
    (args.output / "process-context.json").write_text(json.dumps(process_context, indent=2))
    procs, logs = [], []

    def interrupt(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupt)
    try:
        with ExitStack() as resources:
            runtime = resources.enter_context(tempfile.TemporaryDirectory(prefix="persephone-p0-"))
            root = Path(runtime)
            os.chmod(root, 0o700)
            profiles = []
            names = ("merovingian", "synapse") if args.server == "both" else (args.server,)
            for name in names:
                state = root / name
                state.mkdir(mode=0o700)
                port = free_port()
                base = f"http://127.0.0.1:{port}"
                # Guarantee all classes required by Merovingian's registration
                # policy; random URL-safe text alone may contain no symbol.
                password = "Aa1!" + secrets.token_urlsafe(32)
                registration_auth = {"type": "m.login.dummy"}
                if name == "merovingian":
                    key = state / "master-key"
                    key.write_bytes(secrets.token_bytes(32))
                    key.chmod(0o400)
                    registration = state / "registration-token"
                    registration.write_text(secrets.token_urlsafe(32))
                    registration.chmod(0o400)
                    registration_auth = {"type": "m.login.registration_token", "token": registration.read_text()}
                    config = state / "server.conf"
                    config.write_text("\n".join([
                        "server.name=merovingian.p0.test",
                        # Production requires HTTPS; the synthetic listener is
                        # the loopback HTTP side of a reverse-proxy deployment.
                        f"server.public_baseurl=https://127.0.0.1:{port}",
                        f"listeners.client.bind=127.0.0.1:{port}",
                        "listeners.client.tls=false",
                        "listeners.client.reverse_proxy=true",
                        f"listeners.federation.bind=127.0.0.1:{free_port()}",
                        "listeners.federation.tls=false",
                        "listeners.federation.reverse_proxy=true",
                        "security.federation.enabled=false",
                        "database.backend=sqlite",
                        f"database.sqlite_path={state / 'server.sqlite3'}",
                        f"security.secrets.master_key_file={key}",
                        "security.registration.enabled=true",
                        "security.registration.require_token=true",
                        f"security.registration.token_file={registration}",
                    ]) + "\n")
                    version = subprocess.check_output([str(args.merovingian_binary), "--version"], text=True).strip()
                    command = [str(args.merovingian_binary), "--config", str(config)]
                else:
                    # JSON is valid YAML, avoiding another harness dependency.
                    config = state / "homeserver.yaml"
                    config.write_text(json.dumps({
                        "server_name": "synapse.p0.test",
                        "experimental_features": {"msc3575_enabled": not args.synapse_classic_only},
                        "pid_file": str(state / "synapse.pid"),
                        "public_baseurl": base,
                        "listeners": [{"port": port, "type": "http", "tls": False,
                                       "bind_addresses": ["127.0.0.1"],
                                       "resources": [{"names": ["client"], "compress": False}]}],
                        "database": {"name": "sqlite3", "args": {"database": str(state / "server.db")}},
                        "media_store_path": str(state / "media"),
                        "signing_key_path": str(state / "signing.key"),
                        "report_stats": False,
                        "enable_registration": True,
                        "enable_registration_without_verification": True,
                        "rc_registration": {"per_second": 100, "burst_count": 100},
                        "rc_login": {"address": {"per_second": 100, "burst_count": 100},
                                     "account": {"per_second": 100, "burst_count": 100}},
                        "trusted_key_servers": [],
                        "suppress_key_server_warning": True,
                        "federation_domain_whitelist": [],
                        "macaroon_secret_key": secrets.token_hex(32),
                        "form_secret": secrets.token_hex(32),
                    }, indent=2))
                    version = subprocess.check_output([str(args.synapse_python), "-c", "from importlib.metadata import version; print(version('matrix-synapse'))"], text=True).strip()
                    subprocess.run([str(args.synapse_python), "-m", "synapse.app.homeserver", "--generate-keys", "-c", str(config)], cwd=state, check=True, stdout=subprocess.DEVNULL)
                    command = [str(args.synapse_python), "-m", "synapse.app.homeserver", "-c", str(config)]
                config.chmod(0o600)
                log = (args.output / f"{name}.log").open("wb")
                logs.append(log)
                proc = subprocess.Popen(command, cwd=state, stdout=log, stderr=subprocess.STDOUT)
                procs.append(proc)
                resources.callback(stop, proc)
                versions = wait_ready(base, proc)
                # Credentials exist only in the ignored local profile and the
                # disposable registration request/state.
                recipient_password = "Aa1!" + secrets.token_urlsafe(32)
                register_account(base, name, "p0_alice", password,
                                 registration_auth, args.output)
                register_account(base, name, "p0_bob", recipient_password,
                                 registration_auth, args.output)
                profiles.append({"name": name, "version": version, "base_url": base,
                                 "username": "p0_alice", "password": password,
                                 "recipient": {"username": "p0_bob",
                                               "password": recipient_password}})
                (args.output / f"{name}-versions.json").write_text(json.dumps(versions, indent=2))
                print(f"READY {name} {version} {base}", flush=True)
            profile = args.output / "profile.json"
            profile.write_text(json.dumps(profiles, indent=2))
            profile.chmod(0o600)
            print(f"PROFILE {profile}", flush=True)
            if args.command:
                env = os.environ.copy()
                env["P0_PROFILE"] = str(profile)
                subprocess.run(args.command, env=env, check=True)
            else:
                while True:
                    if any(p.poll() is not None for p in procs):
                        raise RuntimeError("Disposable server stopped unexpectedly")
                    time.sleep(1)
    finally:
        for proc in reversed(procs):
            stop(proc)
        for log in logs:
            log.close()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
