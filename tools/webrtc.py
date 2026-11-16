"""Fetches and verifies the pinned prebuilt libwebrtc archive for the call
worker (ADR-0043). The archive is checked against the SHA-256 in
`tools/webrtc.json` before anything is extracted. Output stays under
`.local/cache/webrtc`. `LK_CUSTOM_WEBRTC` must name the returned directory."""

import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import sys
import tempfile
import urllib.parse
import urllib.request
import zipfile

PINS = Path(__file__).resolve().with_name("webrtc.json")
CHUNK = 1024 * 1024
TIMEOUT = 60
MAX_UNCOMPRESSED = 4 * 1024**3
MARKER = ".verified"
NAME = re.compile(r"webrtc-(win|linux|mac)-(x64|arm64)-release\.zip")
HEX = re.compile(r"[0-9a-f]{64}")
TAG = re.compile(r"[A-Za-z0-9._-]+")


class WebrtcError(Exception):
    pass


def platform_key(system=None, machine=None):
    system = sys.platform if system is None else system
    machine = platform.machine() if machine is None else machine
    operating = {"win32": "win", "darwin": "mac"}.get(system, "linux" if system.startswith("linux") else None)
    architecture = {"amd64": "x64", "x86_64": "x64", "arm64": "arm64", "aarch64": "arm64"}.get(machine.lower())
    if operating is None or architecture is None:
        raise WebrtcError(f"unsupported platform for libwebrtc: {system}/{machine}")
    return f"{operating}-{architecture}"


def load_pins(path=PINS):
    try:
        pins = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise WebrtcError(f"cannot read pins {path}: {error}") from error
    if not isinstance(pins, dict):
        raise WebrtcError("pins must be an object")
    tag, url, assets = pins.get("tag"), pins.get("url"), pins.get("assets")
    if not isinstance(tag, str) or not TAG.fullmatch(tag) or tag in (".", ".."):
        raise WebrtcError("pins: invalid tag")
    if not isinstance(url, str) or "{tag}" not in url or "{name}" not in url:
        raise WebrtcError("pins: url needs {tag} and {name}")
    parts = urllib.parse.urlsplit(url.format(tag=tag, name="x.zip"))
    if parts.scheme != "https" or parts.hostname != "github.com" or parts.port is not None or parts.username:
        raise WebrtcError("pins: url must be https on github.com")
    if not isinstance(assets, dict) or not assets:
        raise WebrtcError("pins: assets must be a non-empty object")
    for key, asset in assets.items():
        if not isinstance(asset, dict) or set(asset) != {"name", "size", "sha256"}:
            raise WebrtcError(f"pins: asset {key} needs name, size and sha256")
        match = NAME.fullmatch(asset["name"]) if isinstance(asset["name"], str) else None
        if not match or key != f"{match.group(1)}-{match.group(2)}":
            raise WebrtcError(f"pins: asset {key} has an invalid name")
        size = asset["size"]
        if not isinstance(size, int) or isinstance(size, bool) or size <= 0:
            raise WebrtcError(f"pins: asset {key} has an invalid size")
        if not isinstance(asset["sha256"], str) or not HEX.fullmatch(asset["sha256"]):
            raise WebrtcError(f"pins: asset {key} has an invalid sha256")
    return pins


def _library_root(directory):
    # ASSUMPTION, not verified against the real archive: the zip holds one
    # top-level folder (such as `win-x64-release/`) and webrtc-sys-build
    # expects LK_CUSTOM_WEBRTC to name that folder. If there is exactly one
    # top-level directory and nothing else, return it; else the extraction dir.
    entries = list(directory.iterdir())
    if len(entries) == 1 and entries[0].is_dir() and not entries[0].is_symlink():
        return entries[0]
    return directory


def _marker_matches(target, sha256):
    try:
        return (target / MARKER).read_text(encoding="utf-8").strip() == sha256 and (target / "files").is_dir()
    except OSError:
        return False


def _default_opener():
    # No proxies from the environment unless the caller passes its own
    # opener. Redirects stay allowed, because GitHub serves assets from
    # another host, but each hop must be https.
    class Https(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            if urllib.parse.urlsplit(newurl).scheme != "https":
                raise WebrtcError(f"refusing non-https redirect to {newurl}")
            return super().redirect_request(req, fp, code, msg, headers, newurl)

    return urllib.request.build_opener(urllib.request.ProxyHandler({}), Https()).open


def _download(url, destination, asset, opener):
    if urllib.parse.urlsplit(url).scheme != "https":
        raise WebrtcError("refusing a download that is not https")
    digest, total = hashlib.sha256(), 0
    with opener(url, timeout=TIMEOUT) as response, open(destination, "wb") as output:
        while True:
            chunk = response.read(CHUNK)
            if not chunk:
                break
            total += len(chunk)
            if total > asset["size"]:
                raise WebrtcError(f"{asset['name']} is larger than the pinned {asset['size']} bytes")
            digest.update(chunk)
            output.write(chunk)
    if total != asset["size"]:
        raise WebrtcError(f"{asset['name']} has {total} bytes, expected {asset['size']}")
    if digest.hexdigest() != asset["sha256"]:
        raise WebrtcError(f"{asset['name']} does not match the pinned SHA-256")


def _extract(archive, destination):
    root = destination.resolve()
    with zipfile.ZipFile(archive) as bundle:
        infos = bundle.infolist()
        if sum(info.file_size for info in infos) > MAX_UNCOMPRESSED:
            raise WebrtcError("archive expands beyond 4 GiB")
        for info in infos:
            name = info.filename
            parts = name.replace("\\", "/").split("/")
            if name.startswith(("/", "\\")) or re.match(r"[A-Za-z]:", name) or ".." in parts:
                raise WebrtcError(f"unsafe path in archive: {name!r}")
            if stat.S_ISLNK(info.external_attr >> 16):
                raise WebrtcError(f"symbolic link in archive: {name!r}")
            target = (root / name).resolve()
            if root != target and root not in target.parents:
                raise WebrtcError(f"unsafe path in archive: {name!r}")
        for info in infos:
            target = root / info.filename
            if info.is_dir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            with bundle.open(info) as source, open(target, "wb") as output:
                shutil.copyfileobj(source, output, CHUNK)


def ensure(root, *, key=None, opener=None, offline=False, pins_path=PINS, announce=print):
    """Returns the directory `LK_CUSTOM_WEBRTC` must name, downloading and
    verifying the archive first unless a verified copy is cached."""
    pins = load_pins(pins_path)
    key = platform_key() if key is None else key
    asset = pins["assets"].get(key)
    if asset is None:
        raise WebrtcError(f"no pinned libwebrtc archive for {key}")
    cache = Path(root) / ".local/cache/webrtc" / pins["tag"]
    target = cache / key
    if _marker_matches(target, asset["sha256"]):
        return _library_root(target / "files")
    if offline:
        raise WebrtcError(f"libwebrtc {pins['tag']} {key} is not cached; run without --offline")
    url = pins["url"].format(tag=pins["tag"], name=asset["name"])
    if urllib.parse.urlsplit(url).scheme != "https":
        raise WebrtcError("refusing a download that is not https")
    cache.mkdir(parents=True, exist_ok=True)
    announce(f"downloading {asset['name']} ({asset['size']} bytes)")
    handle, temporary = tempfile.mkstemp(dir=cache, prefix=f"{key}-", suffix=".part")
    os.close(handle)
    temporary = Path(temporary)
    staging = None
    try:
        _download(url, temporary, asset, opener or _default_opener())
        announce(f"verified {asset['name']}")
        staging = Path(tempfile.mkdtemp(dir=cache, prefix=f"{key}-", suffix=".tmp"))
        (staging / "files").mkdir()
        _extract(temporary, staging / "files")
        (staging / MARKER).write_text(asset["sha256"] + "\n", encoding="utf-8")
        if target.exists():
            shutil.rmtree(target)
        os.replace(staging, target)
        staging = None
    except zipfile.BadZipFile as error:
        raise WebrtcError(f"{asset['name']} is not a valid zip: {error}") from error
    finally:
        temporary.unlink(missing_ok=True)
        if staging is not None:
            shutil.rmtree(staging, ignore_errors=True)
    return _library_root(target / "files")
