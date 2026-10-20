"""Build a disposable optimized/hardened Merovingian binary under .local in WSL."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def source_identity(source):
    head = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    files = subprocess.check_output(["git", "-C", str(source), "ls-files", "-z"]).decode().split("\0")
    digest = hashlib.sha256()
    for name in sorted(files):
        if name in ("meson.build", "meson.options") or name.startswith(("src/", "include/")) or name.endswith(".wrap"):
            digest.update(name.encode() + b"\0")
            digest.update((source / name).read_bytes())
    return {"checkout_head": head, "build_sources_sha256": digest.hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--build", type=Path, required=True)
    parser.add_argument("--jobs", type=int, default=4)
    args = parser.parse_args()
    if sys.platform != "linux":
        raise RuntimeError("Run this helper inside Linux/WSL")
    ignored = Path(__file__).resolve().parents[2] / ".local"
    source, build = args.source.resolve(), args.build.resolve()
    if not build.is_relative_to(ignored.resolve()) or not 1 <= args.jobs <= 8:
        raise ValueError("Build must be under repository .local, with 1..8 jobs")
    before = source_identity(source)
    build.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.update(CC="clang", CXX="clang++")
    setup = ["meson", "setup", str(build), str(source), "--buildtype=release",
             "-Dhardening=true", "-Dbuild_tests=false", "-Dbuild_fuzz=false",
             "--wrap-mode=nodownload", "--force-fallback-for=sqlite3,yyjson"]
    if (build / "meson-private/coredata.dat").exists():
        setup.append("--reconfigure")
    for stage, command in [("setup", setup), ("compile", ["meson", "compile", "-C", str(build), "-j", str(args.jobs), "merovingian-server"])]:
        log = build.parent / f"{build.name}-{stage}.log"
        with log.open("w") as stream:
            result = subprocess.run(command, env=environment, stdout=stream, stderr=subprocess.STDOUT)
        print(f"{stage}: exit={result.returncode}, log={log}", flush=True)
        if result.returncode:
            print("\n".join(log.read_text().splitlines()[-25:]), flush=True)
            return result.returncode
        if stage == "setup":
            commands = json.loads((build / "compile_commands.json").read_text())
            hardening = next(c["command"] for c in commands if c["file"].endswith("hardening_self_check.cpp"))
            if "-O3" not in hardening or "-D_FORTIFY_SOURCE=3" not in hardening:
                raise RuntimeError("Generated self-check compile command lacks optimization/FORTIFY")
    after = source_identity(source)
    if before != after:
        raise RuntimeError("Source changed during compilation; build identity is not stable")
    binary = build / "src/merovingian-server"
    report = {**after, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "version": subprocess.check_output([str(binary), "--version"], text=True).strip(),
              "buildtype": "release", "optimization": "3", "hardening": True,
              "fortify_source": 3, "tests_built": False,
              "compile_command": hardening}
    (build.parent / f"{build.name}-identity.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Built {report['version']}; optimized/FORTIFY verified; source identity stable", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
