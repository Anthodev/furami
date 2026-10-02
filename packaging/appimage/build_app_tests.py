"""Behavioral checks for pinned app build inputs and link provenance."""

import hashlib
import io
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
import os
import struct
import tarfile

SPEC = importlib.util.spec_from_file_location("app_build", Path(__file__).with_name("build-app.py"))
app_build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(app_build)


class AppBuildContractTests(unittest.TestCase):
    def test_changed_prefix_artifact_is_rejected_before_build(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "prefix/bin").mkdir(parents=True)
            (root / "prefix/bin/qmake6").write_bytes(b"tampered Qt")
            media = {"artifacts": {"bin/qmake6": {"type": "file", "sha256": "0" * 64}}}
            with self.assertRaisesRegex(ValueError, "bin/qmake6.*SHA256"):
                app_build.verify_artifact(root / "prefix", media, "bin/qmake6")

    def test_symlink_escape_cannot_load_unverified_build_input(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "prefix/bin").mkdir(parents=True)
            (root / "prefix/bin/qmake6").symlink_to("/usr/bin/qmake6")
            media = {"artifacts": {"bin/qmake6": {"type": "symlink", "target": "/usr/bin/qmake6"}}}
            with self.assertRaisesRegex(ValueError, "outside prefix"):
                app_build.verify_artifact(root / "prefix", media, "bin/qmake6")




    def test_build_macro_and_unselected_target_are_not_shipped_code(self):
        metadata = {
            "packages": [
                {"id": "furami", "targets": [{"kind": ["bin"]}]},
                {"id": "runtime", "targets": [{"kind": ["lib"]}]},
                {"id": "macro", "targets": [{"kind": ["proc-macro"]}]},
                {"id": "macro-dep", "targets": [{"kind": ["lib"]}]},
                {"id": "build-dep", "targets": [{"kind": ["lib"]}]},
                {"id": "windows-only", "targets": [{"kind": ["lib"]}]},
            ],
            "resolve": {"root": "furami", "nodes": [
                {"id": "furami", "deps": [
                    {"pkg": "runtime", "dep_kinds": [{"kind": None}]},
                    {"pkg": "macro", "dep_kinds": [{"kind": None}]},
                    {"pkg": "build-dep", "dep_kinds": [{"kind": "build"}]},
                ]},
                {"id": "runtime", "deps": []},
                {"id": "macro", "deps": [{"pkg": "macro-dep", "dep_kinds": [{"kind": None}]}]},
                {"id": "macro-dep", "deps": []},
                {"id": "build-dep", "deps": []},
                {"id": "windows-only", "deps": []},
            ]},
        }
        roles = app_build.dependency_roles(metadata)
        self.assertEqual(roles["runtime"], "shipped-code-candidate")
        self.assertEqual(roles["macro"], "build-only")
        self.assertEqual(roles["macro-dep"], "build-only")
        self.assertEqual(roles["build-dep"], "build-only")
        self.assertEqual(roles["windows-only"], "not-selected")

    def test_retained_crt_objects_are_attributed_to_pinned_binary_packages(self):
        lock = json.loads(app_build.APP_LOCK.read_text())
        closure = json.loads((app_build.STACK_LOCK.parent / "build-apt-closure.json").read_text())
        linker_map = """
  2fc 2fc 20 4 /usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/Scrt1.o:(.note.ABI-tag)
  92c80 92c80 26 16 /usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/Scrt1.o:(.text)
  92cb0 92cb0 b9 16 /usr/lib/gcc/x86_64-linux-gnu/13/crtbeginS.o:(.text)
  2074b4 2074b4 16 4 /usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/crti.o:(.init)
  2074ca 2074ca 5 1 /usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/crtn.o:(.init)
  219720 219720 0 8 /usr/lib/gcc/x86_64-linux-gnu/13/crtendS.o:(.tm_clone_table)
"""
        objects = app_build.retained_direct_objects(linker_map, lock, closure)
        self.assertEqual({record["path"] for record in objects}, {
            "/usr/lib/x86_64-linux-gnu/Scrt1.o", "/usr/lib/x86_64-linux-gnu/crti.o",
            "/usr/lib/x86_64-linux-gnu/crtn.o", "/usr/lib/gcc/x86_64-linux-gnu/13/crtbeginS.o",
            "/usr/lib/gcc/x86_64-linux-gnu/13/crtendS.o",
        })
        self.assertEqual({record["binary_package"]["name"] for record in objects},
                         {"libgcc-13-dev", "libc6-dev"})
        self.assertEqual({record["source_package"]["name"] for record in objects}, {"gcc-13", "glibc"})
        self.assertTrue(all(record["sha256"] and record["map_path"] for record in objects))
        self.assertFalse(any("libgcc.a" in record["path"] for record in objects))

    def test_unattributed_direct_object_fails_closed_without_claiming_libgcc_archive(self):
        lock = json.loads(app_build.APP_LOCK.read_text())
        closure = json.loads((app_build.STACK_LOCK.parent / "build-apt-closure.json").read_text())
        with self.assertRaisesRegex(ValueError, "unattributed linker object.*unexpected.o"):
            app_build.retained_direct_objects(
                "  2fc 2fc 20 4 /usr/lib/gcc/x86_64-linux-gnu/13/unexpected.o:(.text)\n",
                lock, closure)
        with self.assertRaisesRegex(ValueError, "no retained direct objects"):
            app_build.retained_direct_objects(
                "  2fc 2fc 20 4 /usr/lib/gcc/x86_64-linux-gnu/13/libgcc.a(foo.o):(.text)\n",
                lock, closure)

    def test_rust_standard_library_closure_uses_distinct_archive_source_licenses(self):
        lock = json.loads(app_build.APP_LOCK.read_text())
        linked = [
            "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-aaaa.rlib",
            "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libcore-bbbb.rlib",
            "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/liballoc-eeee.rlib",
            "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libpanic_unwind-ffff.rlib",
            "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libcompiler_builtins-9999.rlib",
            "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libmemchr-cccc.rlib",
            "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/librustc_demangle-7777.rlib",
            "first/target/release/deps/libcxx_qt_macro-dddd.so",
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("libstd-aaaa.rlib", "libcore-bbbb.rlib", "liballoc-eeee.rlib",
                         "libpanic_unwind-ffff.rlib", "libcompiler_builtins-9999.rlib",
                         "libmemchr-cccc.rlib", "librustc_demangle-7777.rlib"):
                archive = root / "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib" / name
                archive.parent.mkdir(parents=True, exist_ok=True)
                archive.write_bytes(name.encode())
            for name, license_name in (("std", "MIT OR Apache-2.0"),
                                       ("core", "MIT OR Apache-2.0"),
                                       ("alloc", "MIT OR Apache-2.0"),
                                       ("panic_unwind", "MIT OR Apache-2.0"),
                                       ("compiler-builtins/compiler-builtins", "MIT OR Apache-2.0"),
                                       ("vendor/memchr-2.7.6", "Unlicense OR MIT"),
                                       ("vendor/rustc-demangle-0.1.27", "MIT/Apache-2.0")):
                manifest = root / "first/rust/lib/rustlib/src/rust/library" / name / "Cargo.toml"
                manifest.parent.mkdir(parents=True, exist_ok=True)
                package_name = ("compiler_builtins" if "compiler-builtins" in name else
                                name.rsplit("/", 1)[-1].split("-2.7")[0].split("-0.1")[0])
                manifest.write_text(f'[package]\nname = "{package_name}"\nlicense = "{license_name}"\n')
            source = root / "sources" / lock["toolchain"]["rust-std"]["archive"]
            source.parent.mkdir()
            with tarfile.open(source, "w:xz") as archive:
                for notice in lock["rust_standard_library"]["notices"]:
                    content = notice["path"].encode()
                    notice["sha256"] = hashlib.sha256(content).hexdigest()
                    item = tarfile.TarInfo(notice["path"])
                    item.size = len(content)
                    archive.addfile(item, io.BytesIO(content))
            result = app_build.rust_std_closure(root, linked, lock)
        self.assertEqual([entry["name"] for entry in result["archives"]],
                         ["std", "core", "alloc", "panic_unwind", "compiler_builtins", "memchr",
                          "rustc_demangle"])
        self.assertEqual(result["archives"][-2]["license_expression"], "Unlicense OR MIT")
        self.assertEqual(result["archives"][-1]["declared_license"], "MIT/Apache-2.0")
        self.assertEqual(result["archives"][-1]["license_expression"], "MIT OR Apache-2.0")
        self.assertEqual(result["source"]["sha256"], lock["toolchain"]["rust-src"]["sha256"])
        self.assertEqual(result["license_expression"], "MIT OR Apache-2.0")
        self.assertEqual({n["path"].rsplit("/", 1)[-1] for n in result["notices"]},
                         {"LICENSE-MIT", "LICENSE-APACHE", "COPYRIGHT"})

    def test_unpinned_crate_source_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            lockfile = Path(directory) / "Cargo.lock"
            lockfile.write_text('version = 4\n[[package]]\nname="bad"\nversion="1.0.0"\nsource="git+https://example.invalid/a"\n')
            with self.assertRaisesRegex(ValueError, "non-registry"):
                app_build.crate_sources(lockfile)

    def test_app_lock_rejects_changed_media_stack_before_download(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            lock = json.loads(app_build.APP_LOCK.read_text())
            lock["media_lock_sha256"] = "0" * 64
            changed = root / "app.lock.json"
            changed.write_text(json.dumps(lock))
            original = app_build.APP_LOCK
            app_build.APP_LOCK = changed
            try:
                with self.assertRaisesRegex(ValueError, "app lock media SHA256"):
                    app_build.build_app(root / "unavailable-media", root / "output", root / "cache")
                self.assertFalse((root / "output").exists())
            finally:
                app_build.APP_LOCK = original

    def test_mismatched_media_manifest_cannot_be_used(self):
        stack = json.loads(app_build.STACK_LOCK.read_text())
        with tempfile.TemporaryDirectory() as directory:
            media = Path(directory)
            (media / "manifest.json").write_text(json.dumps({"schema": 1, "lock_sha256": "0" * 64,
                "container": stack["container"], "apt_snapshot": stack["apt_snapshot"],
                "sources": stack["sources"], "artifacts": {}}))
            with self.assertRaisesRegex(ValueError, "media stack lock"):
                app_build.verify_media(media, stack)


if __name__ == "__main__":
    unittest.main()
