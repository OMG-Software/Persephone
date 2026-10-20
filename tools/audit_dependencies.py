#!/usr/bin/env python3
"""Audit exact registry package versions in the workspace Cargo.lock against OSV."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import shutil
import sys
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


LOCKFILES = ("Cargo.lock",)
OSV_URL = "https://api.osv.dev/v1/querybatch"
OSV_VULN_URL = "https://api.osv.dev/v1/vulns/"
BATCH_SIZE = 100


def load_locked_packages(root: Path) -> list[dict[str, Any]]:
    packages: dict[tuple[str, str], set[str]] = {}
    for relative_path in LOCKFILES:
        lock_path = root / relative_path
        with lock_path.open("rb") as lock_file:
            lock = tomllib.load(lock_file)
        for package in lock.get("package", []):
            source = package.get("source", "")
            if not source.startswith("registry+"):
                continue
            key = (package["name"], package["version"])
            packages.setdefault(key, set()).add(relative_path)

    return [
        {"name": name, "version": version, "lockfiles": sorted(paths)}
        for (name, version), paths in sorted(packages.items())
    ]


def query_batch(packages: list[dict[str, Any]]) -> dict[str, Any]:
    queries = [
        {"package": {"name": package["name"], "ecosystem": "crates.io"}, "version": package["version"]}
        for package in packages
    ]
    request = urllib.request.Request(
        OSV_URL,
        data=json.dumps({"queries": queries}, separators=(",", ":")).encode("utf-8"),
        headers={"Content-Type": "application/json", "Accept": "application/json", "User-Agent": "persephone-p0-osv-audit/1"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        if response.status != 200:
            raise RuntimeError(f"OSV returned HTTP {response.status}")
        body = json.loads(response.read().decode("utf-8"))
    if not isinstance(body, dict) or not isinstance(body.get("results"), list):
        raise RuntimeError("OSV response did not contain a results array")
    return body


def get_advisory(ident: str) -> dict[str, Any]:
    request = urllib.request.Request(
        OSV_VULN_URL + ident,
        headers={"Accept": "application/json", "User-Agent": "persephone-p0-osv-audit/1"},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        if response.status != 200:
            raise RuntimeError(f"OSV returned HTTP {response.status} for {ident}")
        body = json.loads(response.read().decode("utf-8"))
    if not isinstance(body, dict) or body.get("id") != ident:
        raise RuntimeError(f"OSV returned a malformed advisory record for {ident}")
    return body


def advisory_projection(vulnerability: dict[str, Any]) -> dict[str, Any]:
    refs = vulnerability.get("references", [])
    return {
        "id": vulnerability.get("id"),
        "aliases": vulnerability.get("aliases", []),
        "summary": vulnerability.get("summary"),
        "details": vulnerability.get("details"),
        "published": vulnerability.get("published"),
        "modified": vulnerability.get("modified"),
        "withdrawn": vulnerability.get("withdrawn"),
        "references": [ref.get("url") for ref in refs if isinstance(ref, dict) and ref.get("url")],
        "database_specific": vulnerability.get("database_specific", {}),
        "affected": vulnerability.get("affected", []),
    }


def run(root: Path) -> dict[str, Any]:
    packages = load_locked_packages(root)
    report: dict[str, Any] = {
        "schema_version": 1,
        "generated_at_utc": dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat(),
        "source": {"service": "OSV", "endpoint": OSV_URL, "ecosystem": "crates.io", "matching": "exact lockfile name and version"},
        "scope": {"lockfiles": list(LOCKFILES), "registry_package_versions": len(packages), "batch_size": BATCH_SIZE},
        "status": "complete",
        "query_batches": 0,
        "advisory_detail_queries": 0,
        "query_failures": [],
        "advisory_count": 0,
        "packages": [],
        "cargo_audit": {"executed": False, "availability_checked": True, "installed": shutil.which("cargo-audit") is not None},
    }

    for start in range(0, len(packages), BATCH_SIZE):
        batch = packages[start : start + BATCH_SIZE]
        report["query_batches"] += 1
        try:
            body = query_batch(batch)
            results = body["results"]
            if len(results) != len(batch):
                raise RuntimeError(f"OSV returned {len(results)} results for {len(batch)} queries")
            for package, result in zip(batch, results, strict=True):
                if not isinstance(result, dict):
                    raise RuntimeError("OSV result entry was not an object")
                vulns = result.get("vulns", [])
                if not isinstance(vulns, list):
                    raise RuntimeError("OSV result vulns field was not an array")
                advisories = [advisory_projection(vuln) for vuln in vulns if isinstance(vuln, dict)]
                if len(advisories) != len(vulns):
                    raise RuntimeError("OSV returned a malformed advisory entry")
                report["advisory_count"] += len(advisories)
                report["packages"].append({**package, "advisories": advisories})
        except (urllib.error.URLError, TimeoutError, OSError, ValueError, RuntimeError) as error:
            report["status"] = "partial"
            report["query_failures"].append({
                "batch_start_index": start,
                "batch_size": len(batch),
                "error_type": type(error).__name__,
                "message": str(error),
            })
        if start + BATCH_SIZE < len(packages):
            time.sleep(0.2)

    report["queried_package_versions"] = sum(
        len(packages[start : start + BATCH_SIZE])
        for start in range(0, len(packages), BATCH_SIZE)
        if not any(f["batch_start_index"] == start for f in report["query_failures"])
    )
    detail_ids = sorted({
        advisory["id"]
        for package in report["packages"]
        for advisory in package["advisories"]
        if advisory.get("id")
    })
    details: dict[str, dict[str, Any]] = {}
    for ident in detail_ids:
        report["advisory_detail_queries"] += 1
        try:
            details[ident] = advisory_projection(get_advisory(ident))
        except (urllib.error.URLError, TimeoutError, OSError, ValueError, RuntimeError) as error:
            report["status"] = "partial"
            report["query_failures"].append({
                "advisory_id": ident,
                "error_type": type(error).__name__,
                "message": str(error),
            })
    for package in report["packages"]:
        for advisory in package["advisories"]:
            if advisory.get("id") in details:
                advisory.update(details[advisory["id"]])
    report["advisories_found"] = report["advisory_count"] > 0
    return report


def main() -> int:
    default_root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=default_root, help="repository root containing the P0 lockfiles listed in LOCKFILES")
    parser.add_argument("--output", type=Path, default=default_root / "docs/evidence/rust-advisories.json")
    args = parser.parse_args()

    try:
        report = run(args.root.resolve())
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    except Exception as error:  # Keep operational failure distinct from a clean advisory result.
        print(f"OSV audit failed before a complete report: {type(error).__name__}: {error}", file=sys.stderr)
        return 2

    print(f"OSV audit status={report['status']} queried={report['queried_package_versions']}/{report['scope']['registry_package_versions']} advisories={report['advisory_count']} batches={report['query_batches']}")
    print(f"Report: {args.output}")
    return 0 if report["status"] == "complete" else 2


if __name__ == "__main__":
    raise SystemExit(main())
