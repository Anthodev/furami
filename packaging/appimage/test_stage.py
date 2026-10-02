"""Offline integrity checks for qualification assembly (full build exercised separately)."""
import hashlib
import importlib.util
import io
import json
import struct
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("furami_stage", Path(__file__).with_name("stage.py"))
stage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stage)


class StageIntegrityTests(unittest.TestCase):
    def test_comparison_detects_payload_bytes_symlink_and_mode(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            staged, extracted = root / "AppDir", root / "extracted"
            for tree in (staged, extracted):
                (tree / "usr/bin").mkdir(parents=True)
                (tree / "usr/share/applications").mkdir(parents=True)
                (tree / "usr/share/applications/furami.desktop").write_text("[Desktop Entry]\n")
                (tree / "usr/bin/furami").write_bytes(b"binary")
                (tree / "usr/bin/furami").chmod(0o755)
                (tree / "furami.desktop").symlink_to("usr/share/applications/furami.desktop")
            stage.compare_payload(staged, extracted)
            (extracted / "usr/bin/furami").chmod(0o644)
            with self.assertRaisesRegex(ValueError, "payload mismatch"):
                stage.compare_payload(staged, extracted)
            (extracted / "usr/bin/furami").chmod(0o755)
            (extracted / "furami.desktop").unlink()
            (extracted / "furami.desktop").symlink_to("usr/bin/furami")
            with self.assertRaisesRegex(ValueError, "payload mismatch"):
                stage.compare_payload(staged, extracted)
            (extracted / "furami.desktop").unlink()
            (extracted / "furami.desktop").symlink_to("usr/share/applications/furami.desktop")
            (extracted / "usr/bin/furami").write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "payload mismatch"):
                stage.compare_payload(staged, extracted)
            (extracted / "usr/bin/furami").write_bytes(b"binary")
            (extracted / "usr/bin").chmod(0o700)
            with self.assertRaisesRegex(ValueError, "payload mismatch"):
                stage.compare_payload(staged, extracted)

    def test_normalize_appdir_timestamps_including_symlinks(self):
        with tempfile.TemporaryDirectory() as temporary:
            appdir = Path(temporary)
            (appdir / "usr/bin").mkdir(parents=True)
            (appdir / "usr/bin/furami").write_bytes(b"binary")
            (appdir / "furami").symlink_to("usr/bin/furami")
            epoch = 1789862400
            stage.normalize_mtimes(appdir, epoch)
            for path in (appdir, appdir / "usr", appdir / "usr/bin",
                         appdir / "usr/bin/furami", appdir / "furami"):
                self.assertEqual(path.lstat().st_mtime_ns, epoch * 1_000_000_000)

    def test_qt_xcb_context_requires_both_pinned_graphics_integrations(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            prefix, appdir = root / "prefix", root / "AppDir"
            prefix.mkdir()
            appdir.mkdir()
            for relative in stage.XCB_PLUGINS[:2]:
                target = prefix / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(("qualifying " + relative).encode())
            artifacts = {name: value for name, value in stage.snapshot(prefix).items()
                         if value["type"] == "file"}
            with self.assertRaisesRegex(ValueError, "libqxcb-glx-integration.so"):
                stage.stage_xcb_plugins(prefix, artifacts, appdir, {})
            glx = prefix / stage.XCB_PLUGINS[2]
            glx.write_bytes(b"pinned glx")
            artifacts[stage.XCB_PLUGINS[2]] = {"type": "file", "sha256": stage.digest(glx)}
            (prefix / stage.XCB_PLUGINS[1]).write_bytes(b"unrecorded replacement")
            with self.assertRaisesRegex(ValueError, "libqxcb-egl-integration.so"):
                stage.stage_xcb_plugins(prefix, artifacts, appdir, {})

    def test_source_symlinks_must_remain_inside_source_and_match_manifest(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "lib").mkdir()
            (root / "lib/libmpv.so.2").write_bytes(b"mpv")
            (root / "lib/libmpv.so").symlink_to("libmpv.so.2")
            entries = {name: entry for name, entry in stage.snapshot(root).items()
                       if entry["type"] != "directory"}
            stage.verify_artifacts(root, entries)
            (root / "lib/libmpv.so").unlink()
            (root / "lib/libmpv.so").symlink_to("/usr/lib/libmpv.so.2")
            with self.assertRaisesRegex(ValueError, "unsafe symlink"):
                stage.verify_artifacts(root, entries)
            (root / "lib/libmpv.so").unlink()
            (root / "lib/libmpv.so").symlink_to("libmpv.so.2")
            (root / "lib/libmpv.so.2").write_bytes(b"other")
            with self.assertRaisesRegex(ValueError, "artifact mismatch"):
                stage.verify_artifacts(root, entries)

    def test_absolute_runpath_requires_known_prefix_and_room_for_rewrite(self):
        replacement = stage.relocated_runpath("/out/prefix/lib", "usr/lib/qt6/plugins/platforms/libqxcb.so")
        self.assertEqual(replacement, "$ORIGIN")
        with self.assertRaisesRegex(ValueError, "foreign absolute"):
            stage.relocated_runpath("/home/user/build/lib", "usr/bin/furami")
        pulse_path = "/usr/lib/x86_64-linux-gnu/pulseaudio"
        self.assertEqual(stage.relocated_runpath(pulse_path, "usr/lib/libpulse.so.0"),
                         "$ORIGIN/pulseaudio")
        with self.assertRaisesRegex(ValueError, "foreign absolute"):
            stage.relocated_runpath(pulse_path, "usr/lib/libother.so.0")

    def test_pulse_common_nested_lookup_requires_signed_requester(self):
        runpath = [("RUNPATH", "/usr/lib/x86_64-linux-gnu/pulseaudio")]
        self.assertEqual(
            stage.pulse_common_path("libpulsecommon-16.1.so", "usr/lib/libpulse.so.0",
                                    "ubuntu:libpulse0=1:16.1+dfsg1-2ubuntu10.1", runpath),
            ("usr/lib/pulseaudio/libpulsecommon-16.1.so",
             Path("/usr/lib/x86_64-linux-gnu/pulseaudio/libpulsecommon-16.1.so")))
        self.assertIsNone(stage.pulse_common_path("libpulsecommon-16.1.so", "usr/lib/libpulse.so.0",
                                                  "ubuntu:unverified=1", runpath))
        self.assertIsNone(stage.pulse_common_path("libpulsecommon-16.1.so", "usr/lib/libpulse.so.0",
                                                  "ubuntu:libpulse0=1:16.1+dfsg1-2ubuntu10.1", []))
        self.assertIsNone(stage.pulse_common_path("libpulsecommon-16.1.so", "usr/lib/libother.so.0",
                                                  "ubuntu:libpulse0=1:16.1+dfsg1-2ubuntu10.1", runpath))

    def test_runtime_header_accepts_only_pinned_md5_section_write(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runtime = root / "runtime"
            image = root / "image"
            source = bytearray(512)
            source[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<Q", source, 40, 256)  # ELF64 section header table
            struct.pack_into("<HHH", source, 58, 64, 3, 1)
            names = b"\0.shstrtab\0.digest_md5\0"
            source[448:448 + len(names)] = names
            struct.pack_into("<IIQQQQIIQQ", source, 320, 1, 3, 0, 0, 448, len(names), 0, 0, 1, 0)
            struct.pack_into("<IIQQQQIIQQ", source, 384, 11, 1, 0, 0, 192, 32, 0, 0, 16, 0)
            runtime.write_bytes(source)
            packed = bytearray(source)
            packed[192:208] = bytes(range(16))
            image.write_bytes(packed + b"payload")
            report = stage.verify_runtime_header(image, runtime)
            self.assertEqual(report["md5_field"]["offset"], 192)
            self.assertEqual(report["md5_field"]["length"], 16)
            self.assertEqual(report["md5_field"]["before_hex"], "00" * 16)
            self.assertEqual(report["md5_field"]["after_hex"], bytes(range(16)).hex())
            self.assertEqual(report["source_sha256"], stage.digest(runtime))
            self.assertNotEqual(report["image_header_sha256"], report["source_sha256"])
            packed[208] ^= 1  # after the only 16 writable bytes, even inside same section
            image.write_bytes(packed + b"payload")
            with self.assertRaisesRegex(ValueError, "runtime header"):
                stage.verify_runtime_header(image, runtime)
            packed[208] ^= 1
            packed[256] ^= 1  # ELF section table changed; refuse newly described ranges
            image.write_bytes(packed + b"payload")
            with self.assertRaisesRegex(ValueError, "runtime header"):
                stage.verify_runtime_header(image, runtime)

    def test_runtime_header_rejects_missing_or_malformed_digest_section(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runtime = root / "runtime"
            image = root / "image"
            data = bytearray(512)
            data[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<Q", data, 40, 256)
            struct.pack_into("<HHH", data, 58, 64, 3, 5)
            runtime.write_bytes(data)
            image.write_bytes(data + b"payload")
            with self.assertRaisesRegex(ValueError, "section"):
                stage.verify_runtime_header(image, runtime)

    def test_crate_notices_copy_real_vendor_text_not_invented_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            vendor = root / "app/first/vendor/example-1.0.0"
            vendor.mkdir(parents=True)
            (vendor / "LICENSE-MIT").write_text("original author permission\n")
            staged = root / "AppDir"
            staged.mkdir()
            owners = {}
            records = stage.crate_notices(root / "app", {"crates": [
                {"name": "example", "version": "1.0.0", "vendor_path": "first/vendor/example-1.0.0",
                 "license_file": None, "role": "build-only", "license_expression": "MIT",
                 "source": "registry+https://example.invalid", "checksum": "abc"}]}, staged, owners)
            copied = staged / "usr/share/licenses/rust/example-1.0.0/LICENSE-MIT"
            self.assertEqual(copied.read_text(), "original author permission\n")
            self.assertEqual(records[0]["notices"], [copied.relative_to(staged).as_posix()])
            self.assertEqual(owners[copied.relative_to(staged).as_posix()]["source_sha256"], stage.digest(copied))

    def test_linked_kdab_crate_without_top_level_license_preserves_source_attribution(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            vendor = root / "app/first/vendor/cxx-qt-0.10.0"
            (vendor / "src").mkdir(parents=True)
            header = ("// SPDX-FileCopyrightText: 2023 Klarälvdalens Datakonsult AB, a KDAB Group company <info@kdab.com>\n"
                      "// SPDX-FileContributor: Leon Matthes <leon.matthes@kdab.com>\n"
                      "// SPDX-License-Identifier: MIT OR Apache-2.0\n")
            (vendor / "src/lib.rs").write_text(header + "pub fn bridge() {}\n")
            (vendor / "Cargo.toml").write_text('[package]\nname = "cxx-qt"\nversion = "0.10.0"\nlicense = "MIT OR Apache-2.0"\n')
            source = vendor / "src/lib.rs"
            (vendor / ".cargo-checksum.json").write_text(json.dumps({
                "files": {"src/lib.rs": stage.digest(source)}, "package": None}))
            licenses = root / "licenses"
            licenses.mkdir()
            for name in ("MIT.txt", "Apache-2.0.txt"):
                (licenses / name).write_text("upstream " + name + "\n")
            manifest = {"crates": [{
                "name": "cxx-qt", "version": "0.10.0", "role": "shipped-code-candidate",
                "vendor_path": "first/vendor/cxx-qt-0.10.0", "license_expression": "MIT OR Apache-2.0",
                "source": "registry+https://github.com/rust-lang/crates.io-index",
                "checksum": "a" * 64, "license_file": None}]}
            archive = root / "app/sources/cxx-qt-0.10.0.crate"
            archive.parent.mkdir(parents=True)
            with tarfile.open(archive, "w:gz") as bundle:
                bundle.add(vendor / "Cargo.toml", "cxx-qt-0.10.0/Cargo.toml")
                bundle.add(source, "cxx-qt-0.10.0/src/lib.rs")
            original_archive = archive.read_bytes()
            manifest["crates"][0]["checksum"] = stage.digest(archive)
            hashes = {name: stage.digest(licenses / name) for name in ("MIT.txt", "Apache-2.0.txt")}
            staged = root / "AppDir"
            staged.mkdir()
            with patch.dict(stage.KDAB_LICENSE_SHA256, hashes):
                owners = {}
                record, = stage.crate_notices(root / "app", manifest, staged, owners, licenses)
                self.assertEqual(record["license_expression"], "MIT OR Apache-2.0")
                notice = staged / "usr/share/licenses/rust/cxx-qt-0.10.0/SPDX-NOTICES.txt"
                self.assertIn(header.strip(), notice.read_text())
                self.assertIn("src/lib.rs", notice.read_text())
                self.assertEqual(owners[record["notices"][0]]["source_sha256"], stage.digest(notice))
                self.assertEqual(len(record["notices"]), 3)
                (licenses / "MIT.txt").write_text("replaced text\n")
                with self.assertRaisesRegex(ValueError, "upstream KDAB license bytes unavailable/mismatched"):
                    stage.crate_notices(root / "app", manifest, staged, {}, licenses)
                (licenses / "MIT.txt").write_text("upstream MIT.txt\n")
                archive.write_bytes(b"wrong crate")
                with self.assertRaisesRegex(ValueError, "crate archive checksum mismatch"):
                    stage.crate_notices(root / "app", manifest, staged, {}, licenses)
                archive.unlink()
                with self.assertRaisesRegex(ValueError, "crate source archive unavailable"):
                    stage.crate_notices(root / "app", manifest, staged, {}, licenses)
                archive.write_bytes(original_archive)
                source.unlink()
                with self.assertRaisesRegex(ValueError, "crate source unavailable"):
                    stage.crate_notices(root / "app", manifest, staged, {}, licenses)

    def test_linked_crate_without_attribution_fails_but_build_only_macro_may_lack_notice(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            vendor = root / "app/first/vendor/other-1.0.0"
            vendor.mkdir(parents=True)
            appdir = root / "AppDir"
            appdir.mkdir()
            crate = {"name": "other", "version": "1.0.0", "vendor_path": "first/vendor/other-1.0.0",
                     "role": "shipped-code-candidate", "license_file": None, "license_expression": "MIT",
                     "source": "registry+https://example.invalid", "checksum": "a" * 64}
            with self.assertRaisesRegex(ValueError, "linked crate attribution missing"):
                stage.crate_notices(root / "app", {"crates": [crate]}, appdir, {})
            records = stage.crate_notices(root / "app", {"crates": [{**crate, "role": "build-only"}]}, appdir, {})
            self.assertEqual(records[0]["notices"], [])

    def test_rust_std_notices_require_exact_linked_archive_and_distribution(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            app, appdir = root / "app", root / "AppDir"
            (app / "sources").mkdir(parents=True)
            appdir.mkdir()
            linked_path = "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-abcd.rlib"
            linked = app / linked_path
            linked.parent.mkdir(parents=True)
            linked.write_bytes(b"linked upstream standard library")
            vendor_path = "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libmemchr-def.rlib"
            vendor_binary = app / vendor_path
            vendor_binary.write_bytes(b"linked vendored Rust library")
            source_file = app / "sources/rust-src-1.97.1.tar.xz"
            with tarfile.open(source_file, "w:xz") as archive:
                content = b'[package]\nlicense = "MIT OR Apache-2.0"\n'
                member = tarfile.TarInfo(
                    "rust-src-1.97.1/rust-src/lib/rustlib/src/rust/library/std/Cargo.toml")
                member.size = len(content)
                archive.addfile(member, io.BytesIO(content))
                directory = "rust-src-1.97.1/rust-src/lib/rustlib/src/rust/library/vendor/memchr-2.7.6/"
                for name, content in (("Cargo.toml", b'[package]\nlicense = "Unlicense OR MIT"\n'),
                                      ("LICENSE-MIT", b"vendored exact MIT permission\n")):
                    member = tarfile.TarInfo(directory + name)
                    member.size = len(content)
                    archive.addfile(member, io.BytesIO(content))
            distribution = app / "sources/rust-std-1.97.1-x86_64-unknown-linux-gnu.tar.xz"
            prefix = distribution.name.removesuffix(".tar.xz")
            notices = []
            with tarfile.open(distribution, "w:xz") as archive:
                for name in ("LICENSE-APACHE", "LICENSE-MIT", "COPYRIGHT"):
                    content = ("official " + name).encode()
                    member = tarfile.TarInfo(f"{prefix}/{name}")
                    member.size = len(content)
                    archive.addfile(member, io.BytesIO(content))
                    notices.append({"archive": distribution.name, "path": member.name,
                                    "sha256": hashlib.sha256(content).hexdigest()})
            manifest = {"toolchain": {
                "rust-src": {"archive": source_file.name, "sha256": stage.digest(source_file)},
                "rust-std": {"archive": distribution.name, "sha256": stage.digest(distribution)}},
                "binary": {"linked_archives": [linked_path, vendor_path], "rust_standard_library": {
                    "version": "1.97.1", "target": "x86_64-unknown-linux-gnu",
                    "license_expression": "MIT OR Apache-2.0",
                    "source": {"archive": source_file.name, "sha256": stage.digest(source_file),
                               "path": "rust-src-1.97.1/rust-src/lib/rustlib/src/rust/library"},
                    "archives": [{"name": "std", "source_path": "std",
                                  "license_expression": "MIT OR Apache-2.0",
                                  "path": linked_path, "sha256": stage.digest(linked)},
                                 {"name": "memchr", "source_path": "vendor/memchr-2.7.6",
                                  "license_expression": "Unlicense OR MIT",
                                  "path": vendor_path, "sha256": stage.digest(vendor_binary)}],
                    "notices": notices}}}
            owners = {}
            staged = stage.rust_std_notices(app, manifest, appdir, owners)
            self.assertEqual(len(staged["notices"]), 3)
            for destination in staged["notices"]:
                self.assertEqual(stage.digest(appdir / destination), owners[destination]["source_sha256"])
            vendor_notices = staged["vendored_component_notices"]["memchr"]
            self.assertEqual(len(vendor_notices), 1)
            self.assertEqual((appdir / vendor_notices[0]).read_bytes(), b"vendored exact MIT permission\n")
            manifest["binary"]["rust_standard_library"]["notices"][0]["sha256"] = "0" * 64
            with self.assertRaisesRegex(ValueError, "Rust std notice hash mismatch"):
                stage.rust_std_notices(app, manifest, appdir, {})
            manifest["binary"]["rust_standard_library"]["notices"][0]["sha256"] = notices[0]["sha256"]
            linked.write_bytes(b"incorrect linked std")
            with self.assertRaisesRegex(ValueError, "Rust std linked archive bytes mismatch"):
                stage.rust_std_notices(app, manifest, appdir, {})

    def test_runtime_materials_copy_verified_notices_and_real_relink_sources(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runtime, staged = root / "runtime", root / "AppDir"
            runtime.mkdir()
            staged.mkdir()
            names = ("type2-runtime.runtime.c.NOTICE", "gcc.COPYING3",
                     "gcc.COPYING.RUNTIME", "libfuse.LGPL2.txt")
            notice_paths = ["licenses/" + name for name in names]
            sources = ["type2-runtime.tar.gz", "fuse-3.15.0.tar.xz",
                       "mount.c.diff", "squashfuse-0.5.2.tar.gz"]
            relink_paths = ["relink/" + name for name in sources + ["RELINKING.md", "build.sh", "alpine-source.lock.json"]]
            records = {}
            for relative in notice_paths + relink_paths:
                file = runtime / relative
                file.parent.mkdir(exist_ok=True)
                file.write_bytes(("actual:" + relative).encode())
                records[relative] = stage.digest(file)
            lock = {"sources": {key: {"archive": name, "sha256": records["relink/" + name]}
                                for key, name in zip(("type2_runtime", "libfuse", "libfuse_patch", "squashfuse"), sources)}}
            manifest = {"notice_files": {p: records[p] for p in notice_paths},
                        "relink_files": {p: records[p] for p in relink_paths},
                        "files_sha256": records, "sources": lock["sources"],
                        "components": [{"name": "runtime", "notice_files": notice_paths}],
                        "linked_source_closure": {"lock_sha256": records["relink/alpine-source.lock.json"],
                                                  "local_cache_path": str(root / "local-cache"),
                                                  "verified_input_count": 60,
                                                  "distribution_status": "local qualification evidence only; full cache not bundled"}}
            owners = {}
            stage.copy_runtime_materials(runtime, manifest, staged, owners, lock)
            copied = staged / "usr/share/furami/runtime-compliance/relink/fuse-3.15.0.tar.xz"
            self.assertEqual(copied.read_bytes(), (runtime / "relink/fuse-3.15.0.tar.xz").read_bytes())
            self.assertEqual(owners[copied.relative_to(staged).as_posix()]["source_sha256"], records["relink/fuse-3.15.0.tar.xz"])
            missing_notice = {**manifest, "notice_files": {
                key: value for key, value in manifest["notice_files"].items()
                if key != "licenses/gcc.COPYING.RUNTIME"}}
            with self.assertRaisesRegex(ValueError, "runtime notice set incomplete"):
                stage.copy_runtime_materials(runtime, missing_notice, staged, {}, lock)
            missing_source = {**manifest, "relink_files": {
                key: value for key, value in manifest["relink_files"].items()
                if key != "relink/fuse-3.15.0.tar.xz"}}
            with self.assertRaisesRegex(ValueError, "runtime source/patch archive unverified"):
                stage.copy_runtime_materials(runtime, missing_source, staged, {}, lock)
            (runtime / "licenses/gcc.COPYING.RUNTIME").write_text("tampered")
            with self.assertRaisesRegex(ValueError, "runtime licenses bytes/manifest mismatch"):
                stage.copy_runtime_materials(runtime, manifest, staged, {}, lock)


    def test_base_oci_libraries_need_exact_package_and_file_notice_hashes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            library, notice = root / "libstdc++.so.6.0.33", root / "copyright"
            library.write_bytes(b"base library")
            notice.write_bytes(b"base notice")
            provenance = {
                "image": stage.json.loads(stage.STACK_LOCK.read_text())["container"],
                "packages": {"libstdc++6": {"version": "14.2.0-4ubuntu2~24.04.1", "architecture": "amd64"}},
                "files_sha256": {str(path): stage.digest(path) for path in (library, notice)}}
            installed = {("libstdc++6", "14.2.0-4ubuntu2~24.04.1", "amd64")}
            owner = stage.ubuntu_library_owner("libstdc++6", "14.2.0-4ubuntu2~24.04.1",
                                               "amd64", library, notice, installed, {}, provenance)
            self.assertEqual(owner["origin"], "pinned-ubuntu-base-image")
            self.assertEqual(owner["container"], provenance["image"])
            with self.assertRaisesRegex(ValueError, "Ubuntu library package absent from base OCI"):
                stage.ubuntu_library_owner("unknown", "1", "amd64", library, notice,
                                           installed | {("unknown", "1", "amd64")}, {}, provenance)
            library.write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "Ubuntu base library/notice differs"):
                stage.ubuntu_library_owner("libstdc++6", "14.2.0-4ubuntu2~24.04.1",
                                           "amd64", library, notice, installed, {}, provenance)

    def test_epoch_encoded_ubuntu_archive_is_not_misclassified_as_base(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            library, notice = root / "libpulse.so.0", root / "copyright"
            library.write_bytes(b"pulse")
            notice.write_bytes(b"notice")
            owner = stage.ubuntu_library_owner(
                "libpulse0", "1:16.1+dfsg1-2ubuntu10.1", "amd64", library, notice,
                {("libpulse0", "1:16.1+dfsg1-2ubuntu10.1", "amd64")},
                {"libpulse0_1%3a16.1+dfsg1-2ubuntu10.1_amd64.deb": "a" * 64},
                {"image": stage.json.loads(stage.STACK_LOCK.read_text())["container"],
                 "packages": {}, "files_sha256": {}})
            self.assertEqual(owner["origin"], "signed-ubuntu-snapshot-deb")
            self.assertEqual(owner["deb_sha256"], "a" * 64)

if __name__ == "__main__":
    unittest.main()
