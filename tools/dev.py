"""Repository checks and local builds. All generated outputs stay in the ignored .local tree."""

import argparse
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
FRONTEND = ROOT / "frontend"
CONTRACTS = ROOT / "crates/contracts"
# Line coverage of the frontend's TypeScript, for SonarQube
# (sonar.javascript.lcov.reportPaths).
FRONTEND_LCOV = ROOT / ".local/coverage/frontend/lcov.info"
# Line coverage of the Rust crates, for SonarQube (sonar.rust.lcov.reportPaths).
RUST_LCOV = ROOT / ".local/coverage/rust/lcov.info"
# Instrumented builds have their own target directory, so they do not
# invalidate the normal build in .local/build/rust.
COVERAGE_TARGET = ROOT / ".local/build/rust-coverage"
# The cargo-llvm-cov release that docs/development.md and the SonarQube
# workflow install; another may write a different report.
LLVM_COV_VERSION = "0.9.1"
# A source path as a coverage report may name it.
REPORT_PATH = re.compile(r"[A-Za-z0-9_.\-/]+")
# A variable name in `cargo llvm-cov show-env` output.
VARIABLE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def run(command, environment, *, cwd=ROOT):
    print("+ " + " ".join(map(str, command)), flush=True)
    subprocess.run(list(map(str, command)), cwd=cwd, env=environment, check=True)


def repository_paths(lcov, base, root=ROOT, *, absolute=False):
    """Rewrites an LCOV report's source paths as repository-relative POSIX
    paths, so the report reads the same on every OS and SonarQube finds each
    file. Node writes them relative to `base`, the directory the tests ran
    in; cargo-llvm-cov writes them `absolute`, and each must then start with
    `base`. The paths are report text, not paths to open: each must be a
    plain relative path inside `base`, and only text is joined, with no
    lookup."""
    prefix = base.relative_to(root).as_posix()
    absolute_base = base.as_posix().rstrip("/") + "/"
    lines = []
    for line in lcov.read_text(encoding="utf-8").splitlines():
        if line.startswith("SF:"):
            source = line[3:].replace("\\", "/")
            if absolute:
                # Windows drive letters may differ in case.
                if not source.casefold().startswith(absolute_base.casefold()):
                    raise ValueError(f"source path outside {base} in {lcov.name}: {source!r}")
                source = source[len(absolute_base):]
            parts = source.split("/")
            if not REPORT_PATH.fullmatch(source) or any(part in ("", ".", "..") for part in parts):
                raise ValueError(f"unexpected source path in {lcov.name}: {source!r}")
            line = f"SF:{source}" if prefix == "." else f"SF:{prefix}/{source}"
        lines.append(line)
    # The report's text is written back to the same fixed path; nothing from
    # it names a file.
    with lcov.open("w", encoding="utf-8", newline="\n") as report:
        report.write("\n".join(lines) + "\n")


def coverage_environment(environment):
    """`environment` with the variables that `cargo llvm-cov show-env`
    prints, so that plain cargo commands build instrumented workspace crates
    into COVERAGE_TARGET and write their profiles there. The output must be
    `NAME='value'` lines; anything else stops the run."""
    environment = {**environment, "CARGO_TARGET_DIR": str(COVERAGE_TARGET)}
    version = subprocess.run(
        ["cargo", "llvm-cov", "--version"], cwd=ROOT, env=environment, check=True, capture_output=True, text=True
    ).stdout.split()
    if version[-1:] != [LLVM_COV_VERSION]:
        raise SystemExit(f"cargo-llvm-cov {LLVM_COV_VERSION} is required; see docs/development.md")
    shown = subprocess.run(
        ["cargo", "llvm-cov", "show-env"], cwd=ROOT, env=environment, check=True, capture_output=True, text=True
    ).stdout
    for line in shown.splitlines():
        name, separator, value = line.partition("=")
        if len(value) >= 2 and value[0] == value[-1] == "'":
            value = value[1:-1]
        if not separator or not VARIABLE.fullmatch(name) or "'" in value:
            raise ValueError(f"unexpected cargo llvm-cov show-env line: {line!r}")
        environment[name] = value
    return environment


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command",
        choices=("check", "build", "rust", "frontend", "contracts", "coverage", "audit", "bootstrap", "sandbox"),
    )
    parser.add_argument("--offline", action="store_true", help="Use previously cached dependencies")
    parser.add_argument("--release", action="store_true", help="build: use the release profile instead of debug")
    args = parser.parse_args()
    if args.release and args.command != "build":
        parser.error("--release applies only to build")
    environment = os.environ.copy()
    environment["CARGO_HOME"] = str(ROOT / ".local/cache/cargo")
    environment["CARGO_TARGET_DIR"] = str(ROOT / ".local/build/rust")
    environment["npm_config_cache"] = str(ROOT / ".local/cache/npm")
    npm = shutil.which("npm.cmd" if os.name == "nt" else "npm")
    cargo_flags = ["--locked"] + (["--offline"] if args.offline else [])
    # The sandbox tests load this guest module; `rust` lints its wasm-only code.
    wasm_guest = ["-p", "persephone-media-decoder", "--target", "wasm32-unknown-unknown"]
    # Mock-homeserver account, account-settings and registration tests need
    # plain HTTP to loopback, which only the development feature accepts. The
    # default build is what ships.
    dev_core = ["-p", "persephone-core", "--features", "dev-disposable-servers"]
    dev_core_tests = ["--lib", "--test", "account", "--test", "account_settings", "--test", "registration"]

    if args.command in ("check", "rust"):
        # The desktop shell embeds the built frontend at compile time
        # (tauri::generate_context!), so the frontend is built first.
        if not npm:
            parser.error("npm is required to build the frontend the desktop shell embeds")
        run([npm, "run", "build"], environment, cwd=FRONTEND)
        run(["cargo", "fmt", "--all", "--", "--check"], environment)
        run(["cargo", "build", *cargo_flags, "--release", *wasm_guest], environment)
        run(["cargo", "clippy", *cargo_flags, "--release", *wasm_guest, "--", "-D", "warnings"], environment)
        # The broker's end-to-end tests launch this executable, which embeds the
        # guest. `cargo test` does not build another package's binary, so a
        # missing or stale worker would otherwise be used.
        run(["cargo", "build", *cargo_flags, "-p", "persephone-media-worker"], environment)
        run(["cargo", "test", *cargo_flags, "--workspace", "--all-targets"], environment)
        run(["cargo", "clippy", *cargo_flags, "--workspace", "--all-targets", "--", "-D", "warnings"], environment)
        run(["cargo", "test", *cargo_flags, *dev_core, *dev_core_tests], environment)
        run(["cargo", "clippy", *cargo_flags, *dev_core, "--all-targets", "--", "-D", "warnings"], environment)
    if args.command == "coverage":
        # The tests of `rust`, instrumented, writing an LCOV report of the
        # workspace crates. The frontend and the guest are built as for
        # `rust`, uninstrumented: the guest's path is fixed in .local/build/rust.
        if not npm:
            parser.error("npm is required to build the frontend the desktop shell embeds")
        run([npm, "run", "build"], environment, cwd=FRONTEND)
        run(["cargo", "build", *cargo_flags, "--release", *wasm_guest], environment)
        # The worker stays uninstrumented: contained, it may open no file, so
        # it could not write a profile as it exits, and on Linux seccomp would
        # kill it for trying. It is built as for `rust` and copied next to the
        # instrumented tests, which launch it from there. Its own package's
        # tests contain themselves the same way, so they are left out.
        run(["cargo", "build", *cargo_flags, "-p", "persephone-media-worker"], environment)
        coverage = coverage_environment(environment)
        run(["cargo", "llvm-cov", "clean", "--workspace"], coverage)
        workspace = ["--workspace", "--exclude", "persephone-media-worker", "--all-targets"]
        run(["cargo", "test", *cargo_flags, *workspace, "--no-run"], coverage)
        worker = "persephone-media-worker" + (".exe" if os.name == "nt" else "")
        shutil.copy2(ROOT / ".local/build/rust/debug" / worker, COVERAGE_TARGET / "debug" / worker)
        run(["cargo", "test", *cargo_flags, *workspace], coverage)
        run(["cargo", "test", *cargo_flags, *dev_core, *dev_core_tests], coverage)
        RUST_LCOV.parent.mkdir(parents=True, exist_ok=True)
        RUST_LCOV.unlink(missing_ok=True)
        run(["cargo", "llvm-cov", "report", "--lcov", "--output-path", RUST_LCOV], coverage)
        repository_paths(RUST_LCOV, ROOT, absolute=True)
    if args.command == "build":
        if not npm:
            parser.error("npm is required to build the frontend the desktop shell embeds")
        run([npm, "run", "build"], environment, cwd=FRONTEND)
        # The worker embeds the release guest in every profile (see its build.rs).
        run(["cargo", "build", *cargo_flags, "--release", *wasm_guest], environment)
        profile = ["--release"] if args.release else []
        # Separate invocations, as in the release workflow, so that the UI's
        # dependency features are not unified into the core and worker.
        core_and_worker = ["-p", "persephone-core", "--bin", "persephone"]
        core_and_worker += ["-p", "persephone-media-worker", "--bin", "persephone-media-worker"]
        run(["cargo", "build", *cargo_flags, *profile, *core_and_worker], environment)
        run(["cargo", "build", *cargo_flags, *profile, "-p", "persephone-desktop", "--bin", "persephone-ui"], environment)
    if args.command in ("check", "frontend", "bootstrap"):
        if not npm:
            parser.error("npm is required; install Node.js and make npm available on PATH")
        if args.command == "bootstrap":
            run([npm, "ci", "--no-audit", "--no-fund", *(["--offline"] if args.offline else [])], environment, cwd=FRONTEND)
        else:
            for task in ("check", "build"):
                run([npm, "run", task], environment, cwd=FRONTEND)
            # The tests, writing an LCOV report; Node needs its folder.
            FRONTEND_LCOV.parent.mkdir(parents=True, exist_ok=True)
            FRONTEND_LCOV.unlink(missing_ok=True)
            run([npm, "run", "coverage"], environment, cwd=FRONTEND)
            repository_paths(FRONTEND_LCOV, FRONTEND)
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
