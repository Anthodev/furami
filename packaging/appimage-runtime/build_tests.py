#!/usr/bin/env python3
"""Focused safeguards for immutable inputs, static ELF and two-build comparison."""

import hashlib
import importlib.util
import json
import struct
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load_module(name, file):
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


build = load_module("runtime_build", "build.py")
compare = load_module("runtime_compare", "build-twice.py")


class RuntimeRecipeTests(unittest.TestCase):
    def test_changed_apk_closure_fails_before_container_start(self):
        lock = json.loads((HERE / "runtime.lock.json").read_text())
        lock["apk_packages"]["zstd-static"]["sha256"] = "not-a-digest"
        with self.assertRaisesRegex(ValueError, "missing repository or archive SHA256"):
            build.verify_lock(lock)

    def test_source_provenance_cannot_name_different_signed_apk(self):
        lock = json.loads((HERE / "runtime.lock.json").read_text())
        lock["apk_packages"]["gcc"]["sha256"] = "a" * 64
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaisesRegex(ValueError, "gcc-startup: Alpine source does not match"):
                build.verify_alpine_sources(lock, Path(temp))

    def test_relink_rejects_nonabsolute_modified_source_before_fetch(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "new-output"
            cache = Path(temp) / "unused-cache"
            with self.assertRaisesRegex(ValueError, "modified libfuse source must"):
                build.build(output, cache, Path("relative-fuse.tar.xz"))
            self.assertFalse(output.exists())
            self.assertFalse(cache.exists())

    def test_bad_cached_input_does_not_get_replaced_by_network(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "source.tar.gz"
            path.write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "cached source.tar.gz: SHA256"):
                build.obtain("https://example.invalid/source.tar.gz", "a" * 64, path)
            self.assertEqual(path.read_bytes(), b"tampered")

    def test_static_pie_rejects_loader_and_shared_dependencies(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "runtime"
            data = bytearray(256)
            data[:8] = b"\x7fELF\x02\x01\x01\x00"
            data[8:11] = b"AI\x02"
            struct.pack_into("<HH", data, 16, 3, 62)
            struct.pack_into("<Q", data, 32, 64)
            struct.pack_into("<HH", data, 54, 56, 2)
            struct.pack_into("<I", data, 64, 1)  # PT_LOAD
            struct.pack_into("<I", data, 120, 3)  # PT_INTERP
            path.write_bytes(data)
            with self.assertRaisesRegex(ValueError, "PT_INTERP"):
                build.check_elf(path)
            struct.pack_into("<I", data, 120, 2)  # PT_DYNAMIC
            struct.pack_into("<Q", data, 128, 200)  # p_offset
            struct.pack_into("<Q", data, 152, 16)  # p_filesz
            struct.pack_into("<Q", data, 200, 1)  # DT_NEEDED
            path.write_bytes(data)
            with self.assertRaisesRegex(ValueError, "DT_NEEDED"):
                build.check_elf(path)

    def test_unclassified_archive_blocks_runtime_manifest(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "link.map"
            path.write_text("/usr/lib/libcrypto.a(aes.o)\n")
            with self.assertRaisesRegex(ValueError, "unclassified static archive"):
                build.linked_archives(path)

    def test_retained_gcc_startup_is_attributed_without_libgcc_archive(self):
        lock = json.loads((HERE / "runtime.lock.json").read_text())
        gcc = lock["apk_packages"]["gcc"]
        gcc_dir = f"/usr/lib/gcc/x86_64-alpine-linux-musl/{gcc['version'].split('-r', 1)[0]}"
        objects = ["/usr/lib/rcrt1.o", "/usr/lib/crti.o", f"{gcc_dir}/crtbeginS.o",
                   "/tmp/runtime-a1b2c3.o", f"{gcc_dir}/crtendS.o", "/usr/lib/crtn.o"]
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "link.map"
            path.write_text("Linker script and memory map\n" +
                            "".join(f"LOAD {name}\n" for name in objects) +
                            "".join(f" .text  0x1000  0x1 {name}\n" for name in objects))
            linked = build.linked_objects(path, lock)
            self.assertEqual(linked[f"{gcc_dir}/crtbeginS.o"]["component"], "gcc-startup")
            self.assertEqual(linked[f"{gcc_dir}/crtbeginS.o"]["binary_sha256"], gcc["sha256"])
            self.assertEqual(linked["/usr/lib/rcrt1.o"]["binary_apk"], "musl-dev")
            self.assertEqual(linked["runtime.c"]["source_sha256"],
                             lock["sources"]["type2_runtime"]["sha256"])

            # A direct object outside this complete set cannot disappear from
            # the static inventory simply because it is not in an .a archive.
            with path.open("a") as stream:
                stream.write("LOAD /usr/lib/unexpected-startup.o\n")
            with self.assertRaisesRegex(ValueError, "unexpected directly linked objects"):
                build.linked_objects(path, lock)


    def test_manifest_hashes_ignore_unreadable_container_work(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp)
            deliverables = ("runtime-x86_64", "runtime-version.txt", "link.map", "apk-installed.txt",
                            "clang-version.txt", "ld-version.txt", "meson-version.txt", "ninja-version.txt")
            for name in deliverables:
                (output / name).write_bytes(name.encode())
            licenses = output / "licenses"
            licenses.mkdir()
            (licenses / "mimalloc.LICENSE").write_bytes(b"MIT notice")
            relink = output / "relink"
            relink.mkdir()
            (relink / "RELINKING.md").write_bytes(b"source and relink instructions")
            work = output / "work" / "fuse" / "util"
            work.mkdir(parents=True)
            work.chmod(0)
            try:
                records = build.artifact_hashes(output)
                self.assertEqual(records["runtime-x86_64"],
                                 hashlib.sha256(b"runtime-x86_64").hexdigest())
                self.assertEqual(records["licenses/mimalloc.LICENSE"],
                                 hashlib.sha256(b"MIT notice").hexdigest())
                self.assertEqual(records["relink/RELINKING.md"],
                                 hashlib.sha256(b"source and relink instructions").hexdigest())
                self.assertEqual(len(records), len(deliverables) + 2)
            finally:
                work.chmod(0o755)

    def test_comparison_reports_first_changed_runtime_byte(self):
        with tempfile.TemporaryDirectory() as temp:
            first, second = Path(temp) / "first", Path(temp) / "second"
            first.mkdir()
            second.mkdir()
            for name in ("runtime-x86_64", "manifest.json", "link.map", "apk-installed.txt"):
                (first / name).write_bytes(b"same")
                (second / name).write_bytes(b"same")
            (second / "runtime-x86_64").write_bytes(b"saXe")
            self.assertEqual(compare.compare(first, second)["runtime-x86_64"]["first_different_byte"], 2)

    def test_clang_temp_map_variance_is_recorded_but_linkage_change_fails(self):
        with tempfile.TemporaryDirectory() as temp:
            first, second = Path(temp) / "first", Path(temp) / "second"
            first.mkdir()
            second.mkdir()
            for directory, suffix in ((first, "aaaaaa"), (second, "bbbbbb")):
                (directory / "runtime-x86_64").write_bytes(b"same binary")
                (directory / "apk-installed.txt").write_text("same package closure\n")
                (directory / "link.map").write_text(
                    f"/usr/lib/libz.a(z.o)  /tmp/runtime-{suffix}.o (z)\n"
                )
                manifest = {"artifact": {"sha256":compare.digest(directory / "runtime-x86_64")},
                            "files_sha256": {"link.map": compare.digest(directory / "link.map")},
                            "sources": {"commit": "pinned"}}
                (directory / "manifest.json").write_text(json.dumps(manifest, sort_keys=True))
            raw = compare.compare(first, second)
            material, accepted = compare.classify_variance(first, second, raw)
            self.assertEqual(material, {})
            self.assertEqual(accepted["link.map"]["reference_count_each"], 1)
            self.assertEqual(set(raw), {"link.map", "manifest.json"})

            # New archive member is a real linkage difference, not compiler
            # temporary-object noise, even if both runtime bytes still match.
            with (second / "link.map").open("a") as stream:
                stream.write("/usr/lib/libcrypto.a(aes.o)\n")
            manifest = json.loads((second / "manifest.json").read_text())
            manifest["files_sha256"]["link.map"] = compare.digest(second / "link.map")
            (second / "manifest.json").write_text(json.dumps(manifest, sort_keys=True))
            material, accepted = compare.classify_variance(first, second, compare.compare(first, second))
            self.assertEqual(accepted, {})
            self.assertEqual(set(material), {"link.map", "manifest.json"})


if __name__ == "__main__":
    unittest.main()
