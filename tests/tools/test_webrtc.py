"""Fetching and verifying the pinned libwebrtc archive (`tools/webrtc.py`)."""

import hashlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools"))
import webrtc  # noqa: E402

KEY = "win-x64"


def make_zip(entries):
    """A zip of `{name: bytes or ZipInfo-with-data tuple}`."""
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as bundle:
        for name, content in entries.items():
            if isinstance(content, tuple):
                bundle.writestr(content[0], content[1])
            else:
                bundle.writestr(name, content)
    return buffer.getvalue()


def good_zip():
    return make_zip({"win-x64-release/lib/webrtc.lib": b"lib", "win-x64-release/webrtc.ninja": b"n"})


def pins_for(data, **changes):
    asset = {
        "name": "webrtc-win-x64-release.zip",
        "size": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    }
    pins = {
        "tag": "webrtc-test",
        "url": "https://github.com/example/{tag}/{name}",
        "assets": {KEY: asset},
    }
    pins.update(changes)
    return pins


class Opener:
    """Stands in for `urlopen`: serves `data` and counts the calls."""

    def __init__(self, data):
        self.data = data
        self.urls = []

    def __call__(self, url, timeout=None):
        self.urls.append(url)
        assert timeout == webrtc.TIMEOUT
        return io.BytesIO(self.data)


class WebrtcTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)

    def ensure(self, data, pins=None, opener=None, **options):
        pins = pins_for(data) if pins is None else pins
        path = self.root / "pins.json"
        path.write_text(json.dumps(pins), encoding="utf-8")
        opener = Opener(data) if opener is None else opener
        result = webrtc.ensure(
            self.root, key=KEY, opener=opener, pins_path=path, announce=lambda *_: None, **options
        )
        return result, opener

    def cache(self):
        return self.root / ".local/cache/webrtc/webrtc-test"

    def test_given_the_committed_pins_then_they_are_valid_for_every_platform(self):
        pins = webrtc.load_pins()
        self.assertEqual(set(pins["assets"]), {"win-x64", "win-arm64", "linux-x64", "linux-arm64", "mac-x64", "mac-arm64"})

    def test_given_bad_pins_then_loading_is_refused(self):
        valid = pins_for(good_zip())
        asset = valid["assets"][KEY]
        cases = {
            "uppercase hash": {**valid, "assets": {KEY: {**asset, "sha256": "A" * 64}}},
            "short hash": {**valid, "assets": {KEY: {**asset, "sha256": "ab"}}},
            "zero size": {**valid, "assets": {KEY: {**asset, "size": 0}}},
            "boolean size": {**valid, "assets": {KEY: {**asset, "size": True}}},
            "odd name": {**valid, "assets": {KEY: {**asset, "name": "evil.zip"}}},
            "name for another key": {**valid, "assets": {"mac-x64": asset}},
            "extra field": {**valid, "assets": {KEY: {**asset, "extra": 1}}},
            "http url": {**valid, "url": "http://github.com/{tag}/{name}"},
            "other host": {**valid, "url": "https://example.com/{tag}/{name}"},
            "no placeholders": {**valid, "url": "https://github.com/x.zip"},
            "path tag": {**valid, "tag": "../x"},
            "no assets": {**valid, "assets": {}},
        }
        for label, pins in cases.items():
            with self.subTest(label):
                path = self.root / "pins.json"
                path.write_text(json.dumps(pins), encoding="utf-8")
                with self.assertRaises(webrtc.WebrtcError):
                    webrtc.load_pins(path)

    def test_given_known_platforms_then_the_key_is_mapped(self):
        for system, machine, key in (
            ("win32", "AMD64", "win-x64"),
            ("win32", "ARM64", "win-arm64"),
            ("linux", "x86_64", "linux-x64"),
            ("linux", "aarch64", "linux-arm64"),
            ("darwin", "arm64", "mac-arm64"),
            ("darwin", "x86_64", "mac-x64"),
        ):
            with self.subTest(key):
                self.assertEqual(webrtc.platform_key(system, machine), key)

    def test_given_an_unsupported_platform_then_the_key_is_refused(self):
        for system, machine in (("freebsd13", "amd64"), ("linux", "riscv64")):
            with self.subTest(system, machine=machine), self.assertRaises(webrtc.WebrtcError):
                webrtc.platform_key(system, machine)

    def test_given_a_valid_archive_when_ensured_then_it_is_extracted_and_the_inner_folder_returned(self):
        result, opener = self.ensure(good_zip())
        self.assertEqual(result.name, "win-x64-release")
        self.assertEqual((result / "lib/webrtc.lib").read_bytes(), b"lib")
        self.assertEqual(opener.urls, ["https://github.com/example/webrtc-test/webrtc-win-x64-release.zip"])
        self.assertEqual([path.name for path in self.cache().iterdir()], [KEY])

    def test_given_several_top_level_entries_then_the_extraction_directory_is_returned(self):
        result, _ = self.ensure(make_zip({"a/x": b"1", "b/y": b"2"}))
        self.assertEqual(result.name, "files")

    def test_given_a_cached_archive_when_ensured_again_then_nothing_is_downloaded(self):
        data = good_zip()
        first, _ = self.ensure(data)
        second, opener = self.ensure(data)
        self.assertEqual(first, second)
        self.assertEqual(opener.urls, [])

    def test_given_a_marker_for_another_hash_then_the_archive_is_fetched_again(self):
        data = good_zip()
        self.ensure(data)
        (self.cache() / KEY / webrtc.MARKER).write_text("0" * 64, encoding="utf-8")
        _, opener = self.ensure(data)
        self.assertEqual(len(opener.urls), 1)

    def test_given_more_bytes_than_pinned_then_the_download_is_aborted(self):
        data = good_zip()
        pins = pins_for(data)
        pins["assets"][KEY]["size"] = len(data) - 1
        with self.assertRaises(webrtc.WebrtcError) as raised:
            self.ensure(data, pins)
        self.assertIn("larger", str(raised.exception))
        self.assertEqual(list(self.cache().iterdir()), [])

    def test_given_fewer_bytes_than_pinned_then_it_fails(self):
        data = good_zip()
        pins = pins_for(data)
        pins["assets"][KEY]["size"] = len(data) + 1
        with self.assertRaises(webrtc.WebrtcError):
            self.ensure(data, pins)
        self.assertEqual(list(self.cache().iterdir()), [])

    def test_given_a_wrong_hash_then_it_fails_and_leaves_nothing_behind(self):
        data = good_zip()
        pins = pins_for(data)
        pins["assets"][KEY]["sha256"] = "0" * 64
        with self.assertRaises(webrtc.WebrtcError) as raised:
            self.ensure(data, pins)
        self.assertIn("SHA-256", str(raised.exception))
        self.assertEqual(list(self.cache().iterdir()), [])

    def test_given_unsafe_entries_then_extraction_is_refused_and_leaves_nothing_behind(self):
        link = zipfile.ZipInfo("win-x64-release/link")
        link.create_system = 3
        link.external_attr = 0o120777 << 16
        cases = {
            "zip slip": make_zip({"../x": b"x"}),
            "nested slip": make_zip({"a/../../x": b"x"}),
            "absolute": make_zip({"/etc/x": b"x"}),
            "drive letter": make_zip({"C:/x": b"x"}),
            "symlink": make_zip({"link": (link, b"target")}),
        }
        for label, data in cases.items():
            with self.subTest(label), self.assertRaises(webrtc.WebrtcError):
                self.ensure(data)
            self.assertEqual(list(self.cache().iterdir()), [])
            self.assertFalse((self.root / "x").exists())

    def test_given_an_oversized_expansion_then_extraction_is_refused(self):
        data = good_zip()
        original = webrtc.MAX_UNCOMPRESSED
        webrtc.MAX_UNCOMPRESSED = 1
        self.addCleanup(setattr, webrtc, "MAX_UNCOMPRESSED", original)
        with self.assertRaises(webrtc.WebrtcError):
            self.ensure(data)
        self.assertEqual(list(self.cache().iterdir()), [])

    def test_given_offline_and_an_empty_cache_then_it_fails_without_downloading(self):
        data = good_zip()
        with self.assertRaises(webrtc.WebrtcError) as raised:
            self.ensure(data, offline=True)
        self.assertIn("not cached", str(raised.exception))

    def test_given_offline_and_a_cached_archive_then_the_directory_is_returned(self):
        data = good_zip()
        first, _ = self.ensure(data)
        second, opener = self.ensure(data, offline=True)
        self.assertEqual(first, second)
        self.assertEqual(opener.urls, [])

    def test_given_a_tag_or_key_that_leaves_the_cache_then_the_path_is_refused(self):
        base = Path(os.path.realpath(self.root / ".local/cache/webrtc"))
        cache, target = webrtc.cache_dirs(self.root, "webrtc-test", KEY)
        self.assertEqual((cache, target), (base / "webrtc-test", base / "webrtc-test" / KEY))
        for tag, key in [("..", KEY), ("../outside", KEY), ("webrtc-test", ".."), ("webrtc-test", "../../x"), ("a/b", KEY)]:
            with self.subTest(tag=tag, key=key), self.assertRaises(webrtc.WebrtcError):
                webrtc.cache_dirs(self.root, tag, key)

    def test_given_an_http_url_then_nothing_is_downloaded(self):
        data = good_zip()
        pins = pins_for(data, url="http://github.com/{tag}/{name}")
        opener = Opener(data)
        with self.assertRaises(webrtc.WebrtcError):
            self.ensure(data, pins, opener)
        self.assertEqual(opener.urls, [])


if __name__ == "__main__":
    unittest.main()
