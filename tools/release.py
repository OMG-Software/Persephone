"""Set or check the release version in every first-party manifest.

`version X.Y.Z[-pre]` writes the version to src-tauri/tauri.conf.json (the app
version and the MSI version), frontend/package.json, the root entries of
frontend/package-lock.json, the package version of every workspace member
Cargo.toml and the matching first-party Cargo.lock entries. It then confirms
with `cargo metadata --locked --offline` that the lockfile is still valid.

`check-tag vX.Y.Z[-pre]` exits 1 and lists each manifest whose version differs
from the tag. The release workflow runs it before it builds anything.

`bundle-config --notices FILE --output FILE` writes the release-only Tauri
configuration (sidecars and bundled licence files) that the workflow merges
with `cargo tauri --config`. It never adds a signing hook: signing runs in
separate jobs that build nothing (see docs/release.md).

The edits change only the version strings. Each edit is verified by parsing
the file again and comparing it with the expected document, so formatting and
all other values stay as they are.
"""

import argparse
import copy
import glob
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]

_NUMBER = r"(?:0|[1-9]\d*)"
_IDENTIFIER = r"(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)"
# Semantic Versioning 2.0.0 without build metadata, which Cargo, npm and the
# MSI version cannot all carry.
VERSION = re.compile(rf"^({_NUMBER})\.({_NUMBER})\.({_NUMBER})(?:-({_IDENTIFIER}(?:\.{_IDENTIFIER})*))?$")
_JSON_VERSION = re.compile(r'"version"(\s*:\s*)"([^"\\]*)"')
_TOML_VERSION = re.compile(r'^(version\s*=\s*)"([^"\\]*)"', re.MULTILINE)
_TOML_TABLE = re.compile(r"^\[", re.MULTILINE)


class ReleaseError(Exception):
    """A version or manifest problem that stops the command."""


def parse_version(text):
    """Return (version, msi_version) or raise ReleaseError.

    Windows Installer accepts major.minor.build[.revision] with limits of 255,
    255 and 65535. A pre-release must end in a number, which becomes the
    fourth MSI field (0.1.0-alpha.1 -> 0.1.0.1). Windows Installer ignores the
    fourth field when it compares versions, so the MSI shows it but does not
    order pre-releases by it.
    """
    match = VERSION.fullmatch(text)
    if not match:
        raise ReleaseError(f"invalid version {text!r}: use X.Y.Z or X.Y.Z-pre (no build metadata)")
    major, minor, patch, pre = match.groups()
    if int(major) > 255 or int(minor) > 255 or int(patch) > 65535:
        raise ReleaseError(f"version {text!r} exceeds the MSI limits (255.255.65535)")
    if pre is None:
        return text, f"{major}.{minor}.{patch}"
    last = pre.split(".")[-1]
    if not last.isdigit() or int(last) > 65535:
        raise ReleaseError(
            f"pre-release {text!r} must end in a number up to 65535 (for example 0.1.0-alpha.1) "
            "so that the MSI version can carry it"
        )
    return text, f"{major}.{minor}.{patch}.{int(last)}"


def parse_tag(tag):
    if not tag.startswith("v"):
        raise ReleaseError(f"tag {tag!r} must start with 'v'")
    return parse_version(tag[1:])


def _read(path):
    with open(path, encoding="utf-8", newline="") as file:
        return file.read()


def _write(path, text):
    with open(path, "w", encoding="utf-8", newline="") as file:
        file.write(text)


def _get(document, keys):
    for key in keys:
        if not isinstance(document, dict) or key not in document:
            return None
        document = document[key]
    return document


def _set(document, keys, value):
    for key in keys[:-1]:
        document = document[key]
    document[keys[-1]] = value


def _replace_json_version(text, keys, new, where):
    """Change the "version" string at `keys` and leave every other byte as is."""
    document = json.loads(text)
    current = _get(document, keys)
    if not isinstance(current, str):
        raise ReleaseError(f"{where}: {'.'.join(keys) or 'version'} is missing or not a string")
    if current == new:
        return text
    expected = copy.deepcopy(document)
    _set(expected, keys, new)
    for match in _JSON_VERSION.finditer(text):
        if match.group(2) != current:
            continue
        candidate = f'{text[:match.start()]}"version"{match.group(1)}"{new}"{text[match.end():]}'
        if json.loads(candidate) == expected:
            return candidate
    raise ReleaseError(f"{where}: could not locate {'.'.join(keys)} for a text edit")


def _crate_manifests(root):
    workspace = tomllib.loads(_read(root / "Cargo.toml")).get("workspace", {})
    members = workspace.get("members")
    if not members:
        raise ReleaseError("Cargo.toml has no [workspace] members")
    manifests = []
    for pattern in members:
        matches = sorted(glob.glob(pattern, root_dir=root)) if any(c in pattern for c in "*?[") else [pattern]
        for member in matches:
            manifest = root / member / "Cargo.toml"
            if not manifest.is_file():
                raise ReleaseError(f"workspace member {member!r} has no Cargo.toml")
            manifests.append(manifest)
    return manifests


def _crate_version(root, manifest):
    """Return (name, version, inherits) for one workspace member."""
    package = tomllib.loads(_read(manifest)).get("package", {})
    name = package.get("name")
    version = package.get("version")
    if isinstance(version, dict) and version.get("workspace") is True:
        inherited = _get(tomllib.loads(_read(root / "Cargo.toml")), ["workspace", "package", "version"])
        return name, inherited, True
    if not isinstance(name, str) or not isinstance(version, str):
        raise ReleaseError(f"{manifest.relative_to(root).as_posix()}: [package] needs a literal name and version")
    return name, version, False


def _replace_toml_version(text, table, keys, new, where):
    """Change `version = "..."` inside `[table]` and verify the parsed result."""
    document = tomllib.loads(text)
    current = _get(document, keys)
    if current == new:
        return text
    expected = copy.deepcopy(document)
    _set(expected, keys, new)
    header = re.search(rf"^\[{re.escape(table)}\][ \t]*(?:#[^\r\n]*)?\r?$", text, re.MULTILINE)
    if not header:
        raise ReleaseError(f"{where}: no [{table}] table")
    following = _TOML_TABLE.search(text, header.end())
    end = following.start() if following else len(text)
    version = _TOML_VERSION.search(text, header.end(), end)
    if not version:
        raise ReleaseError(f"{where}: [{table}] has no literal version")
    candidate = f'{text[:version.start()]}{version.group(1)}"{new}"{text[version.end():]}'
    if tomllib.loads(candidate) != expected:
        raise ReleaseError(f"{where}: the version edit changed more than [{table}] version")
    return candidate


def _lock_versions(text):
    """First-party (path) packages in Cargo.lock: name -> version."""
    packages = tomllib.loads(text).get("package", [])
    return {p["name"]: p["version"] for p in packages if "source" not in p}


def _replace_lock_versions(text, names, new):
    """Set the version of first-party lock entries and their references."""
    document = tomllib.loads(text)
    expected = copy.deepcopy(document)
    old_by_name = {}
    for package in expected.get("package", []):
        if "source" not in package and package["name"] in names:
            old_by_name[package["name"]] = package["version"]
            package["version"] = new
    for package in expected.get("package", []):
        dependencies = package.get("dependencies", [])
        for index, dependency in enumerate(dependencies):
            name, _, version = dependency.partition(" ")
            if version and name in old_by_name and version == old_by_name[name]:
                dependencies[index] = f"{name} {new}"
    blocks = re.split(r"(?m)^(?=\[\[package\]\]\r?$)", text)
    edited = []
    for block in blocks:
        name = re.search(r'^name = "([^"]+)"\r?$', block, re.MULTILINE)
        if block.startswith("[[package]]") and name and name.group(1) in old_by_name:
            if re.search(r"^source = ", block, re.MULTILINE):
                edited.append(block)
                continue
            block = re.sub(r'^version = "[^"]*"(?=\r?$)', f'version = "{new}"', block, count=1, flags=re.MULTILINE)
        for dep_name, old in old_by_name.items():
            block = block.replace(f' "{dep_name} {old}",', f' "{dep_name} {new}",')
        edited.append(block)
    candidate = "".join(edited)
    if tomllib.loads(candidate) != expected:
        raise ReleaseError("Cargo.lock: the version edit changed more than first-party versions")
    return candidate


def current_versions(root):
    """Every version the release must agree on, as [(place, version)]."""
    places = []
    tauri = json.loads(_read(root / "src-tauri/tauri.conf.json"))
    places.append(("src-tauri/tauri.conf.json version", _get(tauri, ["version"])))
    places.append(("src-tauri/tauri.conf.json bundle.windows.wix.version (MSI)", _get(tauri, ["bundle", "windows", "wix", "version"])))
    package = json.loads(_read(root / "frontend/package.json"))
    places.append(("frontend/package.json version", _get(package, ["version"])))
    lock = json.loads(_read(root / "frontend/package-lock.json"))
    places.append(("frontend/package-lock.json version", _get(lock, ["version"])))
    places.append(('frontend/package-lock.json packages[""].version', _get(lock, ["packages", "", "version"])))
    names = []
    for manifest in _crate_manifests(root):
        name, version, _ = _crate_version(root, manifest)
        names.append(name)
        places.append((f"{manifest.relative_to(root).as_posix()} ({name})", version))
    locked = _lock_versions(_read(root / "Cargo.lock"))
    for name in names:
        places.append((f"Cargo.lock {name}", locked.get(name)))
    return places


def check(root, tag):
    """Return the places whose version differs from the tag."""
    version, msi = parse_tag(tag)
    mismatches = []
    for place, found in current_versions(root):
        wanted = msi if "(MSI)" in place else version
        if found != wanted:
            mismatches.append((place, found, wanted))
    return mismatches


def set_version(root, new):
    """Write `new` to every first-party manifest under `root`."""
    version, msi = parse_version(new)
    edits = {}

    path = root / "src-tauri/tauri.conf.json"
    text = _replace_json_version(_read(path), ["version"], version, path.name)
    edits[path] = _replace_json_version(text, ["bundle", "windows", "wix", "version"], msi, path.name)

    path = root / "frontend/package.json"
    edits[path] = _replace_json_version(_read(path), ["version"], version, "frontend/package.json")

    path = root / "frontend/package-lock.json"
    text = _replace_json_version(_read(path), ["version"], version, "frontend/package-lock.json")
    edits[path] = _replace_json_version(text, ["packages", "", "version"], version, "frontend/package-lock.json")

    names = []
    inherits = False
    for manifest in _crate_manifests(root):
        name, _, inherited = _crate_version(root, manifest)
        names.append(name)
        if inherited:
            inherits = True
            continue
        where = manifest.relative_to(root).as_posix()
        edits[manifest] = _replace_toml_version(_read(manifest), "package", ["package", "version"], version, where)
    if inherits:
        path = root / "Cargo.toml"
        edits[path] = _replace_toml_version(
            edits.get(path, _read(path)), "workspace.package", ["workspace", "package", "version"], version, "Cargo.toml"
        )

    path = root / "Cargo.lock"
    edits[path] = _replace_lock_versions(_read(path), set(names), version)

    # Every edit is computed and verified before any file is written.
    for path, text in edits.items():
        if _read(path) != text:
            _write(path, text)


def cargo_lock_is_valid(root):
    """Confirm with Cargo that the lockfile still matches the manifests."""
    command = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"]
    environment = os.environ.copy()
    # Use the dependency cache that tools/dev.py fills, when there is one.
    if (root / ".local/cache/cargo").is_dir():
        environment["CARGO_HOME"] = str(root / ".local/cache/cargo")
    result = subprocess.run(command, cwd=root, env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    if result.returncode:
        print(result.stderr, file=sys.stderr, end="")
    return result.returncode == 0


SIDECARS = ("binaries/persephone", "binaries/persephone-media-worker")


def bundle_config(root, notices):
    """The release-only Tauri configuration that `cargo tauri --config` merges.

    It stays out of tauri.conf.json because tauri-build copies `externalBin`
    and `resources` on every build and fails when they are missing, which
    would break development builds and tests.
    """
    tauri_dir = root / "src-tauri"
    notices = Path(notices).resolve()
    if not notices.is_file():
        raise ReleaseError(f"third-party notices not found at {notices}")
    config = {
        "bundle": {
            # tauri.conf.json keeps bundling off for development builds.
            "active": True,
            "externalBin": list(SIDECARS),
            "resources": {
                os.path.relpath(root / "LICENSE", tauri_dir).replace(os.sep, "/"): "LICENSE.txt",
                os.path.relpath(notices, tauri_dir).replace(os.sep, "/"): "THIRD-PARTY-NOTICES.txt",
            },
        }
    }
    return config


def main(argv=None, root=ROOT):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("version", help="set the release version").add_argument("version")
    commands.add_parser("check-tag", help="compare every version with a vX.Y.Z tag").add_argument("tag")
    bundle = commands.add_parser("bundle-config", help="write the release-only Tauri configuration")
    bundle.add_argument("--notices", required=True, help="third-party notices file to bundle")
    bundle.add_argument("--output", required=True, help="JSON file for `cargo tauri --config`")
    args = parser.parse_args(argv)
    try:
        if args.command == "bundle-config":
            config = bundle_config(root, args.notices)
            output = Path(args.output)
            output.parent.mkdir(parents=True, exist_ok=True)
            _write(output, json.dumps(config, indent=2) + "\n")
            return 0
        if args.command == "version":
            set_version(root, args.version)
            if not cargo_lock_is_valid(root):
                print("cargo metadata --locked --offline rejected the edited Cargo.lock", file=sys.stderr)
                return 1
            print(f"Set version {args.version}; review the diff and commit it.")
            return 0
        mismatches = check(root, args.tag)
    except ReleaseError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    for place, found, wanted in mismatches:
        print(f"{place}: {found!r}, expected {wanted!r}", file=sys.stderr)
    if mismatches:
        print(f"{len(mismatches)} version(s) differ from {args.tag}", file=sys.stderr)
        return 1
    print(f"All versions match {args.tag}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
