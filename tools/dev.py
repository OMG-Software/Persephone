"""Repository checks. All generated outputs stay in the ignored .local tree."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
FRONTEND = ROOT / "frontend"
CONTRACTS = ROOT / "crates/contracts"


def run(command, environment, *, cwd=ROOT):
    print("+ " + " ".join(map(str, command)), flush=True)
    subprocess.run(list(map(str, command)), cwd=cwd, env=environment, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "rust", "frontend", "contracts", "audit", "bootstrap", "sandbox"))
    parser.add_argument("--offline", action="store_true", help="Use previously cached dependencies")
    args = parser.parse_args()
    environment = os.environ.copy()
    environment["CARGO_HOME"] = str(ROOT / ".local/cache/cargo")
    environment["CARGO_TARGET_DIR"] = str(ROOT / ".local/build/rust")
    environment["npm_config_cache"] = str(ROOT / ".local/cache/npm")
    npm = shutil.which("npm.cmd" if os.name == "nt" else "npm")
    cargo_flags = ["--locked"] + (["--offline"] if args.offline else [])

    if args.command in ("check", "rust"):
        run(["cargo", "fmt", "--all", "--", "--check"], environment)
        # The sandbox tests load this guest module; its wasm-only code is linted here.
        wasm_guest = ["-p", "persephone-media-decoder", "--target", "wasm32-unknown-unknown"]
        run(["cargo", "build", *cargo_flags, "--release", *wasm_guest], environment)
        run(["cargo", "clippy", *cargo_flags, "--release", *wasm_guest, "--", "-D", "warnings"], environment)
        run(["cargo", "test", *cargo_flags, "--workspace", "--all-targets"], environment)
        run(["cargo", "clippy", *cargo_flags, "--workspace", "--all-targets", "--", "-D", "warnings"], environment)
    if args.command in ("check", "frontend", "bootstrap"):
        if not npm:
            parser.error("npm is required; install Node.js and make npm available on PATH")
        if args.command == "bootstrap":
            run([npm, "ci", "--no-audit", "--no-fund", *(["--offline"] if args.offline else [])], environment, cwd=FRONTEND)
        else:
            for task in ("check", "build", "test"):
                run([npm, "run", task], environment, cwd=FRONTEND)
    if args.command in ("check", "contracts"):
        run(["cargo", "run", *cargo_flags, "-p", "persephone-contracts", "--bin", "export-contracts", "--", "--check"], environment)
        # Run the pinned local compiler without npx downloading another version.
        run(["node", FRONTEND / "node_modules/typescript/bin/tsc", "--project", CONTRACTS / "tsconfig.json"], environment)
    if args.command == "check":
        run([sys.executable, "-m", "unittest", "discover", "-s", "tests/servers", "-v"], environment)
        run([sys.executable, "-m", "unittest", "discover", "-s", "tests/tools", "-v"], environment)
    if args.command == "audit":
        if args.offline:
            parser.error("audit queries OSV and cannot run offline")
        run([sys.executable, ROOT / "tools/audit_dependencies.py"], environment)
    if args.command == "sandbox":
        if os.name != "nt":
            parser.error("the current sandbox probe requires Windows; other platform probes remain open")
        run(["cargo", "build", *cargo_flags, "--release", "-p", "persephone-test-windows"], environment)
        environment["PERSEPHONE_PROBE_DIR"] = str(ROOT / ".local/runs" / f"windows-{time.time_ns()}-{os.getpid()}")
        # Preserve the native probe's nonzero gate result; never turn it into a pass.
        run([ROOT / ".local/build/rust/release/persephone-test-windows.exe"], environment)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.returncode)
    except FileNotFoundError as error:
        print(f"Required development tool is unavailable: {error.filename}", file=sys.stderr)
        raise SystemExit(2)
