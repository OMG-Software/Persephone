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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--revision", help="Check historical source blobs at this Git revision")
    parser.add_argument("--markdown", nargs="*", default=[])
    parser.add_argument("--json", nargs="*", default=[])
    args = parser.parse_args()
    manifest = json.loads(repository_file(args.manifest).read_text(encoding="utf-8"))
    for name, expected in manifest["files"].items():
        path = repository_file(name)
        if args.revision:
            content = subprocess.check_output(
                ["git", "show", f"{args.revision}:{path.relative_to(ROOT).as_posix()}"], cwd=ROOT
            )
        else:
            content = path.read_bytes()
        actual = hashlib.sha256(content.replace(b"\r\n", b"\n")).hexdigest()
        if actual != expected:
            raise ValueError(f"Source hash mismatch: {name}")
    for name in args.json:
        json.loads(repository_file(name).read_text(encoding="utf-8"))
    for name in args.markdown:
        path = repository_file(name)
        text = re.sub(r"```[\s\S]*?```", "", path.read_text(encoding="utf-8"))
        for target in re.findall(r"\[[^\]]*\]\(([^\s)]+)\)", text):
            if re.match(r"[a-zA-Z][a-zA-Z0-9+.-]*:", target) or target.startswith("#"):
                continue
            local = unquote(target.strip("<>").split("#", 1)[0])
            if local and not (path.parent / local).exists():
                raise ValueError(f"Missing local Markdown target: {name}: {target}")
    subprocess.run(["git", "diff", "--check"], cwd=ROOT, check=True)
    print(f"PASS: {len(manifest['files'])} source hashes, {len(args.json)} JSON files, "
          f"{len(args.markdown)} Markdown files; diff hygiene.")


if __name__ == "__main__":
    main()
