"""Check selected evidence hashes, JSON files and local Markdown links."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parents[1]


def repository_file(name):
    """Refuse paths outside the checkout, including symlink escapes."""
    path = (ROOT / name).resolve()
    path.relative_to(ROOT)
    return path


def reject_constant(value):
    """Evidence uses JSON numbers, never Python's non-finite extensions."""
    raise ValueError(f"Invalid JSON constant: {value}")


def read_json(path):
    """Apply strict JSON parsing to manifests and explicitly checked files."""
    return json.loads(path.read_text(encoding="utf-8"), parse_constant=reject_constant)


def resolve_revision(revision):
    """Resolve one commit before using it in a Git object/path argument."""
    if not revision or revision.startswith("-") or "\0" in revision:
        raise ValueError("Expected a commit revision, not a Git option")
    oid = subprocess.check_output(
        ["git", "rev-parse", "--verify", "--end-of-options", f"{revision}^{{commit}}"],
        cwd=ROOT,
        text=True,
    ).strip()
    if not re.fullmatch(r"(?:[a-fA-F0-9]{40}|[a-fA-F0-9]{64})", oid):
        raise ValueError("Git did not return one commit object ID")
    return oid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--revision", help="Check historical source blobs at this Git revision")
    parser.add_argument("--markdown", nargs="*", default=[])
    parser.add_argument("--json", nargs="*", default=[])
    args = parser.parse_args()
    manifest = read_json(repository_file(args.manifest))
    revision = resolve_revision(args.revision) if args.revision is not None else None
    for name, expected in manifest["files"].items():
        path = repository_file(name)
        if revision:
            content = subprocess.check_output(
                ["git", "show", "--no-ext-diff", "--no-textconv", "--end-of-options",
                 f"{revision}:{path.relative_to(ROOT).as_posix()}"], cwd=ROOT
            )
        else:
            content = path.read_bytes()
        actual = hashlib.sha256(content.replace(b"\r\n", b"\n")).hexdigest()
        if actual != expected:
            raise ValueError(f"Source hash mismatch: {name}")
    for name in args.json:
        read_json(repository_file(name))
    for name in args.markdown:
        path = repository_file(name)
        text = re.sub(r"```[\s\S]*?```", "", path.read_text(encoding="utf-8"))
        links = r'''\[[^\]]*\]\(\s*(<[^>]*>|[^\s)]+)(?:\s+(?:"[^"]*"|'[^']*'|\([^)]*\)))?\s*\)'''
        for target in re.findall(links, text):
            target = target.strip("<>")
            if re.match(r"[a-zA-Z][a-zA-Z0-9+.-]*:", target) or target.startswith("#"):
                continue
            local = unquote(target.split("#", 1)[0])
            if local and not (path.parent / local).exists():
                raise ValueError(f"Missing local Markdown target: {name}: {target}")
    subprocess.run(["git", "diff", "--check"], cwd=ROOT, check=True)
    print(f"PASS: {len(manifest['files'])} source hashes, {len(args.json)} JSON files, "
          f"{len(args.markdown)} Markdown files; unstaged tracked diff whitespace.")


if __name__ == "__main__":
    main()
