"""Behavioral checks for pinned app build inputs and link provenance."""

import hashlib
import io
import importlib.util
import json
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace
import contextlib
from pathlib import Path
import os
import struct
import tarfile

SPEC = importlib.util.spec_from_file_location("app_build", Path(__file__).with_name("build-app.py"))
app_build = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(app_build)


class WorkspaceCopyIntegrityTests(unittest.TestCase):
    def test_destination_copy_must_remain_regular_and_hash_matching(self):
        for fault in ("changed", "symlink", "directory"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                snapshot = root / "snapshot"
                snapshot.mkdir()
                source = snapshot / "input"
                source.write_bytes(b"recorded")
                inputs = {"input": hashlib.sha256(b"recorded").hexdigest()}

                def faulty_copy(source, target):
                    if fault == "changed":
                        target.write_bytes(b"corrupted copy")
                    elif fault == "symlink":
                        target.symlink_to(source)
                    else:
                        target.mkdir()

                with patch.object(app_build.shutil, "copy2", side_effect=faulty_copy), \
                        self.assertRaises(ValueError):
                    app_build.prepare_build_workspace(snapshot, root / "workspace", inputs)
                self.assertEqual(source.read_bytes(), b"recorded")


class BuildWorkspaceTests(unittest.TestCase):
    def fixture(self, root):
        snapshot = root / "app/source-snapshot"
        inputs = {}
        for relative, content in {
            "Cargo.toml": b"recorded manifest",
            "src/main.rs": b"recorded source",
            "packaging/media/build-apt-closure.json": b"recorded closure",
        }.items():
            path = snapshot / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
            inputs[relative] = hashlib.sha256(content).hexdigest()
        return snapshot, inputs

    def test_clean_and_dirty_checkouts_build_only_recorded_snapshot_inputs(self):
        for dirty in (False, True):
            with self.subTest(dirty=dirty), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                snapshot, inputs = self.fixture(root)
                checkout = root / "checkout"
                checkout.mkdir()
                if dirty:
                    for relative in (".qmlls.ini", ".cargo/config.toml", "unrelated"):
                        path = checkout / relative
                        path.parent.mkdir(parents=True, exist_ok=True)
                        path.write_text("private developer artifact")
                (snapshot / "unrecorded").write_text("not an input either")
                media = root / "media"
                for relative in ("bin/qmake6", "libexec/rcc"):
                    path = media / "prefix" / relative
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(b"tool")
                stack = json.loads(app_build.STACK_LOCK.read_text())
                workspace = root / "app/first/workspace"
                checkout_before = {str(p.relative_to(checkout)): p.read_bytes()
                                   for p in checkout.rglob("*") if p.is_file()}

                def build(command, **kwargs):
                    files = {p.relative_to(workspace).as_posix()
                             for p in workspace.rglob("*") if p.is_file()}
                    self.assertEqual(files, set(inputs))
                    self.assertIn(f"{workspace}:/workspace:rw", command)
                    self.assertNotIn(f"{checkout}:/workspace:ro", command)
                    self.assertNotIn(f"{checkout}:/workspace:rw", command)
                    for mount in (f"{media}:/out:ro", f"{media}:/frozen-provider:ro",
                                  f"{root / 'sources'}:/sources:ro",
                                  f"{root / 'app/first'}:/build",
                                  f"{root / 'app/first/wrappers/rcc'}:/out/prefix/libexec/rcc:ro"):
                        self.assertIn(mount, command)
                    self.assertEqual(command[command.index("--workdir") + 1], "/workspace")
                    (workspace / ".qmlls.ini").write_text(
                        '[General]\nbuildDir="/build/target/qml_modules"\nno-cmake-calls=true\n')
                    binary = root / "app/first/bin/furami"
                    binary.parent.mkdir()
                    binary.write_bytes(b"fixture executable")
                    (root / "app/first/evidence/furami.link.map").write_text("map")
                    return SimpleNamespace(returncode=0)

                with patch.object(app_build, "REPOSITORY", checkout), \
                        patch.object(app_build.subprocess, "run", side_effect=build):
                    binary = app_build.run_build(
                        media, root / "app", root / "sources", stack, "first", inputs)
                self.assertEqual(binary.read_bytes(), b"fixture executable")
                self.assertTrue((workspace / ".qmlls.ini").read_text())
                self.assertFalse((snapshot / ".qmlls.ini").exists())
                self.assertEqual(checkout_before, {
                    str(p.relative_to(checkout)): p.read_bytes()
                    for p in checkout.rglob("*") if p.is_file()})
                for relative, expected in inputs.items():
                    self.assertEqual(app_build.sha256(snapshot / relative), expected)

    def test_bad_snapshot_inputs_fail_before_child(self):
        for fault in ("missing", "changed", "file-symlink", "parent-symlink", "directory",
                      "bad-hash", "absolute", "dot", "dot-part", "parent", "empty"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                snapshot, inputs = self.fixture(root)
                path = snapshot / "src/main.rs"
                if fault == "missing":
                    path.unlink()
                elif fault == "changed":
                    path.write_bytes(b"changed")
                elif fault == "file-symlink":
                    path.unlink()
                    path.symlink_to(snapshot / "Cargo.toml")
                elif fault == "parent-symlink":
                    path.unlink()
                    path.parent.rmdir()
                    path.parent.symlink_to(root, target_is_directory=True)
                elif fault == "directory":
                    path.unlink()
                    path.mkdir()
                elif fault == "bad-hash":
                    inputs["src/main.rs"] = "not a digest"
                else:
                    relative = {"absolute": "/outside", "dot": ".", "dot-part": "src/./main.rs",
                                "parent": "../outside", "empty": ""}[fault]
                    inputs[relative] = "0" * 64
                with patch.object(app_build.subprocess, "run") as child, \
                        self.assertRaises(ValueError):
                    app_build.run_build(root / "media", root / "app", root / "sources",
                                        {}, "first", inputs)
                child.assert_not_called()

    def test_workspace_requires_fresh_nonsymlink_destination(self):
        for fault in ("existing", "symlink", "parent-symlink"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                snapshot, inputs = self.fixture(root)
                workspace = root / "build/workspace"
                workspace.parent.mkdir()
                if fault == "existing":
                    workspace.mkdir()
                elif fault == "symlink":
                    workspace.symlink_to(root / "missing")
                else:
                    workspace.parent.rmdir()
                    workspace.parent.symlink_to(snapshot, target_is_directory=True)
                with self.assertRaises(ValueError):
                    app_build.prepare_build_workspace(snapshot, workspace, inputs)

    def test_successful_child_cannot_mutate_recorded_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            snapshot, inputs = self.fixture(root)
            media = root / "media"
            for relative in ("bin/qmake6", "libexec/rcc"):
                path = media / "prefix" / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"tool")

            def mutate(command, **kwargs):
                (root / "app/first/workspace/src/main.rs").write_bytes(b"changed by child")
                return SimpleNamespace(returncode=0)

            with patch.object(app_build.subprocess, "run", side_effect=mutate), \
                    self.assertRaisesRegex(ValueError, "workspace.*SHA256"):
                app_build.run_build(media, root / "app", root / "sources",
                                    json.loads(app_build.STACK_LOCK.read_text()), "first", inputs)

    def test_input_allowlist_includes_locked_apt_closure(self):
        inputs = app_build.input_hashes()
        self.assertEqual(inputs["packaging/media/build-apt-closure.json"],
                         app_build.sha256(app_build.STACK_LOCK.parent / "build-apt-closure.json"))


class AppBuildContractTests(unittest.TestCase):
    def test_failed_build_tail_refuses_symlinked_log_or_parent(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            private = root / "private"
            private.mkdir()
            (private / "build.log").write_text("PRIVATE HOST DATA")
            direct = root / "build.log"
            direct.symlink_to(private / "build.log")
            alias = root / "alias"
            alias.symlink_to(private, target_is_directory=True)
            for log in (direct, alias / "build.log"):
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    app_build.emit_failed_build_log(log)
                self.assertNotIn("PRIVATE HOST DATA", stderr.getvalue())
                self.assertIn("tail unavailable", stderr.getvalue())

    def test_failed_build_tail_is_bounded_and_missing_log_is_nonfatal(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "build.log"
            log.write_bytes(b"OMITTED HEAD\n" + b"x\n" * 200 + b"z" * 20000 + b"\nfinal error\n")
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                app_build.emit_failed_build_log(log)
                app_build.emit_failed_build_log(log.with_name("missing"))
            text = stderr.getvalue()
            self.assertNotIn("OMITTED HEAD", text)
            self.assertIn("final error", text)
            self.assertIn("tail unavailable", text)
            self.assertLess(len(text), 17000)
            # The line bound also applies when the byte bound is not reached.
            log.write_text("".join(f"line {i}\n" for i in range(200)))
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                app_build.emit_failed_build_log(log)
            self.assertNotIn("line 0\n", stderr.getvalue())
            self.assertIn("line 199\n", stderr.getvalue())
            self.assertLessEqual(len(stderr.getvalue().splitlines()), 122)

    def test_unavailable_diagnostics_cannot_replace_container_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            media, output = root / "media", root / "app"
            output.mkdir()
            (output / "source-snapshot").mkdir()
            (output / "source-snapshot/Cargo.toml").write_bytes(b"fixture")
            inputs = {"Cargo.toml": hashlib.sha256(b"fixture").hexdigest()}
            for name in ("bin/qmake6", "libexec/rcc"):
                tool = media / "prefix" / name
                tool.parent.mkdir(parents=True, exist_ok=True)
                tool.write_bytes(b"fixture")
            stack = json.loads(app_build.STACK_LOCK.read_text())

            def fail(command, **kwargs):
                (output / "first/build.log").unlink()
                return SimpleNamespace(returncode=37)

            class ClosedStderr:
                def write(self, text):
                    raise OSError("closed stderr")

            with patch.object(app_build.subprocess, "run", side_effect=fail), \
                    contextlib.redirect_stderr(ClosedStderr()), \
                    self.assertRaisesRegex(RuntimeError, "container exited 37"):
                app_build.run_build(media, output, root / "sources", stack, "first", inputs)

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
Merging program properties

Discarded input sections

 .data          0x0000000000000000        0x4 /usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/Scrt1.o
 .bss           0x0000000000000000        0x0 /usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/Scrt1.o

Memory Configuration

Name             Origin             Length             Attributes
*default*        0x0000000000000000 0xffffffffffffffff

Linker script and memory map

LOAD /usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/Scrt1.o
LOAD /usr/lib/gcc/x86_64-linux-gnu/13/crtbeginS.o
LOAD /usr/lib/gcc/x86_64-linux-gnu/13/libgcc.a

.note.ABI-tag   0x00000000000002fc       0x20
 .note.ABI-tag  0x00000000000002fc       0x20 /usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/Scrt1.o

.init           0x000000000000033c       0x1b
 *(.init)
 .init          0x000000000000033c       0x16 /usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/crti.o
 .init          0x0000000000000352        0x5 /usr/lib/gcc/x86_64-linux-gnu/13/../../../x86_64-linux-gnu/crtn.o

.text           0x0000000000000380       0xf4
 *(.text.unlikely .text.*_unlikely .text.unlikely.*)
 .text          0x0000000000000380       0xb9 /usr/lib/gcc/x86_64-linux-gnu/13/crtbeginS.o
 .text          0x0000000000000800       0x26 /build/target/release/deps/furami.o

.tm_clone_table 0x0000000000003008        0x0
 .tm_clone_table
                0x0000000000003008        0x8 /usr/lib/gcc/x86_64-linux-gnu/13/crtendS.o
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
                "Linker script and memory map\n"
                " .text          0x0000000000000380       0x26"
                " /usr/lib/gcc/x86_64-linux-gnu/13/unexpected.o\n",
                lock, closure)
        with self.assertRaisesRegex(ValueError, "no retained direct objects"):
            app_build.retained_direct_objects(
                "Linker script and memory map\n"
                "LOAD /usr/lib/gcc/x86_64-linux-gnu/13/libgcc.a\n"
                " .text          0x0000000000000380       0x26"
                " /usr/lib/gcc/x86_64-linux-gnu/13/libgcc.a(foo.o)\n",
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

    def test_old_unpatched_provider_is_rejected_before_app_output_or_download(self):
        stack = json.loads(app_build.STACK_LOCK.read_text())
        stack["sources"]["ffmpeg"].pop("patches", None)
        previous_lock = hashlib.sha256(json.dumps(stack, sort_keys=True).encode()).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            media = root / "old-media"
            media.mkdir()
            original = json.dumps({"schema": 1, "lock_sha256": previous_lock,
                                   "container": stack["container"], "apt_snapshot": stack["apt_snapshot"],
                                   "sources": stack["sources"], "artifacts": {}})
            (media / "manifest.json").write_text(original)
            with patch.object(app_build, "obtain", side_effect=AssertionError("must not download")):
                with self.assertRaisesRegex(ValueError, "media stack lock"):
                    app_build.build_app(media, root / "app", root / "cache")
            self.assertFalse((root / "app").exists())
            self.assertEqual((media / "manifest.json").read_text(), original)

    def test_pristine_provider_cannot_claim_new_lock_without_source_patch_metadata(self):
        stack = json.loads(app_build.STACK_LOCK.read_text())
        pristine = json.loads(json.dumps(stack["sources"]))
        pristine["ffmpeg"].pop("patches", None)
        with tempfile.TemporaryDirectory() as directory:
            media = Path(directory)
            (media / "manifest.json").write_text(json.dumps({
                "schema": 1, "lock_sha256": app_build.sha256(app_build.STACK_LOCK),
                "container": stack["container"], "apt_snapshot": stack["apt_snapshot"],
                "sources": pristine, "artifacts": {}}))
            with self.assertRaisesRegex(ValueError, "media source versions"):
                app_build.verify_media(media, stack)

    def test_provider_requires_applied_patch_receipt_and_original_snapshot_bytes(self):
        stack = json.loads(app_build.STACK_LOCK.read_text())
        with tempfile.TemporaryDirectory() as directory:
            media = Path(directory)
            manifest = {"schema": 1, "lock_sha256": app_build.sha256(app_build.STACK_LOCK),
                        "sources": stack["sources"]}
            with self.assertRaisesRegex(ValueError, "receipt missing"):
                app_build.source_patches.verify_provider(media, manifest, stack["sources"])
            app_build.source_patches.snapshot(
                media / "source-inputs", app_build.STACK_LOCK.read_bytes(),
                app_build.source_patches.inputs(app_build.STACK_LOCK.parent, stack["sources"]))
            receipt = media / "source-patches.json"
            receipt.write_text(json.dumps(app_build.source_patches.receipt(stack["sources"])))
            manifest["source_patches"] = {"path": receipt.name, "sha256": app_build.sha256(receipt)}
            app_build.source_patches.verify_provider(media, manifest, stack["sources"])
            row = app_build.source_patches.entries(stack["sources"])[0]
            supplied = media / "source-inputs" / row["file"]
            supplied.chmod(0o644)
            supplied.write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "source input SHA256"):
                app_build.source_patches.verify_provider(media, manifest, stack["sources"])

    def test_source_snapshot_explicitly_includes_locked_patch_bytes(self):
        stack = json.loads(app_build.STACK_LOCK.read_text())
        actual = app_build.input_hashes()
        for row in app_build.source_patches.entries(stack["sources"]):
            self.assertEqual(actual["packaging/media/" + row["file"]], row["sha256"])


if __name__ == "__main__":
    unittest.main()
