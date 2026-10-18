"""Focused consumer-visible checks; run with python3 -m unittest discover -s packaging/media -p 'build_tests.py'."""

import importlib.util
import hashlib
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("media_build", Path(__file__).with_name("build.py"))
media_build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(media_build)
QT_SPEC = importlib.util.spec_from_file_location("official_qt_kit", Path(__file__).with_name("build-qt-kit.py"))
qt_kit = importlib.util.module_from_spec(QT_SPEC)
QT_SPEC.loader.exec_module(qt_kit)
PATCH_SPEC = importlib.util.spec_from_file_location("media_source_patches", Path(__file__).with_name("source-patches.py"))
source_patches = importlib.util.module_from_spec(PATCH_SPEC)
PATCH_SPEC.loader.exec_module(source_patches)


class SourceTests(unittest.TestCase):
    def test_corrupt_cached_archive_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            (cache / "mpv.tar.gz").write_bytes(b"damaged")
            with self.assertRaisesRegex(ValueError, "mpv.*SHA256"):
                media_build.obtain_source(
                    "mpv", {"url": "https://example.invalid/source", "sha256": "a" * 64,
                            "archive": "mpv.tar.gz"}, cache
                )

    def test_rejects_cached_qt_archive_when_published_sha1_disagrees(self):
        import hashlib
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            payload = b"qt archive bytes"
            (cache / "qtbase.7z").write_bytes(payload)
            with self.assertRaisesRegex(ValueError, "qtbase.*SHA1"):
                media_build.obtain_source(
                    "qtbase", {"url": "https://example.invalid/qtbase.7z",
                               "sha256": hashlib.sha256(payload).hexdigest(),
                               "upstream_sha1": "0" * 40, "archive": "qtbase.7z"}, cache
                )

    def test_missing_or_unverified_source_lock_is_rejected(self):
        lock = json.loads(media_build.LOCK_PATH.read_text())
        lock["sources"]["libplacebo"]["sha256"] = ""
        with self.assertRaisesRegex(ValueError, "libplacebo.*SHA256"):
            media_build.check_lock(lock)

    def test_missing_transitive_package_closure_is_rejected(self):
        lock = json.loads(media_build.LOCK_PATH.read_text())
        lock["apt_closure"]["installed_sha256"] = ""
        with self.assertRaisesRegex(ValueError, "APT closure"):
            media_build.check_lock(lock)

    def test_missing_mpv_client_api_pin_is_rejected(self):
        lock = json.loads(media_build.LOCK_PATH.read_text())
        lock["sources"]["mpv"]["client_api"] = ""
        with self.assertRaisesRegex(ValueError, "mpv client API"):
            media_build.check_lock(lock)

    def test_rejects_mpv_opt_in_to_optional_libjpeg_writer(self):
        lock = json.loads(media_build.LOCK_PATH.read_text())
        lock["sources"]["mpv"]["build_flags"] = ["-Djpeg=enabled"]
        with self.assertRaisesRegex(ValueError, "libjpeg image writer"):
            media_build.check_lock(lock)

    def test_rejects_unpinned_official_qt_archive(self):
        lock = json.loads(media_build.LOCK_PATH.read_text())
        lock["sources"]["qt"]["archives"][0]["sha256"] = ""
        with self.assertRaisesRegex(ValueError, "Qt.*SHA256"):
            media_build.check_lock(lock)

    def test_rejects_non_official_qt_archive_url(self):
        lock = json.loads(media_build.LOCK_PATH.read_text())
        lock["sources"]["qt"]["archives"][0]["url"] = "https://example.invalid/qtbase.7z"
        with self.assertRaisesRegex(ValueError, "Qt.*official"):
            media_build.check_lock(lock)


class SourcePatchTests(unittest.TestCase):
    def fixture(self, root):
        recipe = root / "recipe"
        (recipe / "patches").mkdir(parents=True)
        target = recipe / "patches/first.patch"
        target.write_bytes(b"diff --git a/value.txt b/value.txt\n--- a/value.txt\n+++ b/value.txt\n"
                           b"@@ -1 +1 @@\n-before\n+after\n")
        sources = {"ffmpeg": {"patches": [{"file": "patches/first.patch",
                                          "sha256": media_build.sha256(target)}]}}
        return recipe, target, sources

    def test_verified_snapshot_is_applied_in_order_not_live_patch_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            recipe, first, sources = self.fixture(root)
            second = recipe / "patches/second.patch"
            second.write_bytes(first.read_bytes().replace(b"-before\n+after", b"-after\n+final"))
            sources["ffmpeg"]["patches"].append(
                {"file": "patches/second.patch", "sha256": media_build.sha256(second)})
            lock_bytes = json.dumps({"sources": sources}).encode()
            snapshot = root / "source-inputs"
            source_patches.snapshot(snapshot, lock_bytes, source_patches.inputs(recipe, sources))
            first.write_bytes(b"untrusted later bytes")
            tree = root / "ffmpeg"
            tree.mkdir()
            (tree / "value.txt").write_text("before\n")
            receipt = root / "receipt.json"
            source_patches.apply(snapshot, tree, receipt, hashlib.sha256(lock_bytes).hexdigest())
            self.assertEqual((tree / "value.txt").read_text(), "final\n")
            self.assertEqual(json.loads(receipt.read_text()), source_patches.receipt(sources))
            self.assertEqual(snapshot.joinpath("patches/first.patch").read_bytes(),
                             b"diff --git a/value.txt b/value.txt\n--- a/value.txt\n+++ b/value.txt\n"
                             b"@@ -1 +1 @@\n-before\n+after\n")

    def test_tampered_missing_symlink_and_parent_symlink_inputs_fail_closed(self):
        for mutation in ("tampered", "missing", "symlink", "parent-symlink"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                recipe, target, sources = self.fixture(root)
                original = target.read_bytes()
                if mutation == "tampered":
                    target.write_bytes(b"tampered")
                elif mutation == "missing":
                    target.unlink()
                else:
                    external = root / "external"
                    external.mkdir()
                    (external / target.name).write_bytes(original)
                    target.unlink()
                    if mutation == "symlink":
                        target.symlink_to(external / target.name)
                    else:
                        target.parent.rmdir()
                        target.parent.symlink_to(external, target_is_directory=True)
                with self.assertRaises(ValueError):
                    source_patches.inputs(recipe, sources)

    def test_invalid_metadata_cannot_escape_recipe_or_patch_another_component(self):
        for relative in ("../escape.patch", "/absolute.patch", "patches/../escape.patch",
                         "patches//x.patch", "patches/./x.patch", "patches\\x.patch"):
            with self.subTest(relative=relative), self.assertRaises(ValueError):
                source_patches.entries({"ffmpeg": {"patches": [{"file": relative, "sha256": "a" * 64}]}})
        with tempfile.TemporaryDirectory() as temporary:
            _, _, sources = self.fixture(Path(temporary))
            entry = sources["ffmpeg"]["patches"][0]
            for value in (None, {}, [entry, entry], [{**entry, "extra": True}],
                          [{**entry, "sha256": ""}]):
                with self.subTest(value=value), self.assertRaises(ValueError):
                    source_patches.entries({"ffmpeg": {"patches": value}})
            with self.assertRaises(ValueError):
                source_patches.entries({"mpv": sources["ffmpeg"]})

    def test_patch_application_failure_or_changed_snapshot_has_no_success_receipt(self):
        for mutation in ("context", "patch-hash", "lock-hash"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                recipe, _, sources = self.fixture(root)
                lock_bytes = json.dumps({"sources": sources}).encode()
                snapshot, tree, receipt = root / "snapshot", root / "ffmpeg", root / "receipt.json"
                source_patches.snapshot(snapshot, lock_bytes, source_patches.inputs(recipe, sources))
                tree.mkdir()
                (tree / "value.txt").write_text("wrong context\n" if mutation == "context" else "before\n")
                expected = hashlib.sha256(lock_bytes).hexdigest()
                if mutation == "patch-hash":
                    file = snapshot / "patches/first.patch"
                    file.chmod(0o644)
                    file.write_bytes(b"tampered")
                elif mutation == "lock-hash":
                    expected = "0" * 64
                with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                    source_patches.apply(snapshot, tree, receipt, expected)
                self.assertFalse(receipt.exists())
                self.assertEqual((tree / "value.txt").read_text(),
                                 "wrong context\n" if mutation == "context" else "before\n")

    def test_diff_traversal_cannot_modify_a_file_outside_extracted_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            recipe, supplied, sources = self.fixture(root)
            supplied.write_bytes(supplied.read_bytes().replace(b"value.txt", b"../outside.txt"))
            sources["ffmpeg"]["patches"][0]["sha256"] = media_build.sha256(supplied)
            lock_bytes = json.dumps({"sources": sources}).encode()
            snapshot, tree, receipt = root / "snapshot", root / "ffmpeg", root / "receipt.json"
            source_patches.snapshot(snapshot, lock_bytes, source_patches.inputs(recipe, sources))
            tree.mkdir()
            outside = root / "outside.txt"
            outside.write_text("before\n")
            with self.assertRaises(subprocess.CalledProcessError):
                source_patches.apply(snapshot, tree, receipt, hashlib.sha256(lock_bytes).hexdigest())
            self.assertEqual(outside.read_text(), "before\n")
            self.assertFalse(receipt.exists())

    def test_builder_rejects_patch_tampering_before_output_or_download(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            recipe, target, sources = self.fixture(root)
            lock = json.loads(media_build.LOCK_PATH.read_text())
            lock["sources"]["ffmpeg"]["patches"] = sources["ffmpeg"]["patches"]
            pinned = recipe / "stack.lock.json"
            pinned.write_text(json.dumps(lock))
            target.write_bytes(b"tampered")
            with patch.object(media_build, "LOCK_PATH", pinned), \
                    patch.object(media_build, "obtain_source", side_effect=AssertionError("must not download")):
                with self.assertRaisesRegex(ValueError, "SHA256"):
                    media_build.build(root / "output", root / "cache")
            self.assertFalse((root / "output").exists())


class QtMetadataTests(unittest.TestCase):
    def test_relocates_official_pkgconfig_prl_and_libtool_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix = Path(directory)
            pc_dir = prefix / "lib/pkgconfig"
            pc_dir.mkdir(parents=True)
            (prefix / "bin").mkdir()
            pc = pc_dir / "Qt6Core.pc"
            pc.write_text("prefix=/home/qt/work/install\nVersion: 6.11.2\n")
            prl = prefix / "lib/libQt6Core.prl"
            prl.write_text("QMAKE_PRL_LIBS = -L/home/qt/work/install/lib -lfoo\n")
            la = prefix / "lib/libQt6Core.la"
            la.write_text("libdir='/home/qt/work/install/lib'\n")

            changes = qt_kit.relocate_metadata(prefix)

            self.assertIn(f"prefix={prefix}\n", pc.read_text())
            self.assertIn("$$[QT_INSTALL_LIBS]", prl.read_text())
            self.assertIn(f"libdir='{prefix}/lib'", la.read_text())
            self.assertEqual((prefix / "bin/qt.conf").read_text(), "[Paths]\nPrefix=..\n")
            self.assertEqual({row["path"] for row in changes}, {
                "lib/pkgconfig/Qt6Core.pc", "lib/libQt6Core.prl",
                "lib/libQt6Core.la", "bin/qt.conf"
            })
            self.assertNotEqual(changes[0]["original_sha256"], changes[0]["installed_sha256"])

    def test_rejects_unknown_qt_pkgconfig_prefix(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix = Path(directory)
            pc_dir = prefix / "lib/pkgconfig"
            pc_dir.mkdir(parents=True)
            (prefix / "bin").mkdir()
            (pc_dir / "Qt6Core.pc").write_text("prefix=/usr/lib\nVersion: 6.11.2\n")
            with self.assertRaisesRegex(ValueError, "Qt pkg-config prefix"):
                qt_kit.relocate_metadata(prefix)


class DeviceListingTests(unittest.TestCase):
    def test_accepts_input_only_and_input_output_alias_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            listing = Path(directory) / "ffmpeg-devices.txt"
            listing.write_text("Devices:\n D  video4linux2,v4l2 Video4Linux2 device grab\n DE pulse Pulse audio input\n")
            result = subprocess.run(
                ["bash", str(Path(__file__).with_name("build-device-check.sh")), str(listing)],
                capture_output=True, text=True
            )
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_output_only_video_and_substring_alias(self):
        with tempfile.TemporaryDirectory() as directory:
            listing = Path(directory) / "ffmpeg-devices.txt"
            listing.write_text(" E  video4linux2,v4l2 Video4Linux2 device output\n"
                               " D  other-v4l2 Video4Linux2 lookalike\n D  pulse Pulse audio input\n")
            result = subprocess.run(
                ["bash", str(Path(__file__).with_name("build-device-check.sh")), str(listing)],
                capture_output=True, text=True
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("v4l2", result.stderr)

    def test_rejects_output_only_pulse(self):
        with tempfile.TemporaryDirectory() as directory:
            listing = Path(directory) / "ffmpeg-devices.txt"
            listing.write_text(" D  video4linux2,v4l2 Video4Linux2 device grab\n"
                               " E  pulse Pulse audio output\n")
            result = subprocess.run(
                ["bash", str(Path(__file__).with_name("build-device-check.sh")), str(listing)],
                capture_output=True, text=True
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("pulse", result.stderr)


class ReproducibilityTests(unittest.TestCase):
    def test_changed_and_extra_deliverables_fail_comparison(self):
        with tempfile.TemporaryDirectory() as directory:
            first, second = Path(directory) / "first", Path(directory) / "second"
            first.mkdir()
            second.mkdir()
            (first / "libmpv.so").write_bytes(b"first")
            (second / "libmpv.so").write_bytes(b"second")
            with self.assertRaisesRegex(ValueError, "libmpv.so"):
                media_build.compare_deliverables(first, second)
            (second / "libmpv.so").write_bytes(b"first")
            (second / "libavcodec.so").write_bytes(b"extra")
            with self.assertRaisesRegex(ValueError, "libavcodec.so"):
                media_build.compare_deliverables(first, second)

    def test_symlink_target_is_part_of_artifact_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            first, second = Path(directory) / "first", Path(directory) / "second"
            first.mkdir()
            second.mkdir()
            (first / "libmpv.so").symlink_to("libmpv.so.2")
            (second / "libmpv.so").symlink_to("libmpv.so.3")
            with self.assertRaisesRegex(ValueError, "libmpv.so"):
                media_build.compare_deliverables(first, second)


if __name__ == "__main__":
    unittest.main()
