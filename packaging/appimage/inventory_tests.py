#!/usr/bin/env python3
"""Behavioral regression tests for internal AppImage audit helpers."""

import hashlib
import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("media_inventory", Path(__file__).with_name("inventory.py"))
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)


class InventoryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name) / "prefix"
        self.root.mkdir()

    def file(self, path, data=b"ELF contents"):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        return target

    def metadata(self, path, needed=(), runpath=None, rpath=None):
        return {"path": path, "needed": list(needed), "runpath": runpath, "rpath": rpath,
                "class": "ELF64", "machine": "Advanced Micro Devices X86-64"}

    def test_soname_alias_hashes_resolved_target(self):
        target = self.file("lib/libmpv.so.2.5.0")
        (self.root / "lib/libmpv.so.2").symlink_to(target.name)
        (self.root / "lib/libmpv.so").symlink_to("libmpv.so.2")
        self.file("bin/mpv")
        files, links = inventory.walk_tree(str(self.root))
        hashes = {f["path"]: f["sha256"] for f in files}
        entries = inventory.resolve_dependency_closure(
            str(self.root), [self.metadata("bin/mpv", ["libmpv.so.2"], "$ORIGIN/../lib"),
                             self.metadata("lib/libmpv.so.2.5.0")],
            hashes, links, [], [])
        self.assertEqual(entries[0]["resolution"]["path"], "lib/libmpv.so.2")
        self.assertEqual(entries[0]["resolution"]["resolved_target"], "lib/libmpv.so.2.5.0")
        self.assertEqual(entries[0]["resolution"]["file_sha256"], hashes["lib/libmpv.so.2.5.0"])

    def test_outside_and_unsearched_alias_cannot_claim_bundled(self):
        self.file("private/libsame.so.1")
        (self.root / "private/liboutside.so.1").symlink_to("/usr/lib/liboutside.so.1")
        entries = [self.metadata("bin/player", ["libsame.so.1", "liboutside.so.1"], "$ORIGIN/../lib"),
                   self.metadata("private/libsame.so.1")]
        self.file("bin/player")
        files, links = inventory.walk_tree(str(self.root))
        warnings = []
        deps = inventory.resolve_dependency_closure(
            str(self.root), entries,
            {f["path"]: f["sha256"] for f in files}, links, [], warnings)
        self.assertEqual({d["name"]: d["resolution"]["kind"] for d in deps},
                         {"libsame.so.1": "missing", "liboutside.so.1": "missing"})
        self.assertEqual(deps[0]["resolution"]["unsearched_staged_candidates"],
                         ["private/libsame.so.1"])
        self.assertTrue(any("not in loader search paths" in warning for warning in warnings))

    def test_distinct_runpaths_resolve_same_soname_per_requester(self):
        alpha = self.file("one/libsame.so.1", b"one")
        beta = self.file("two/libsame.so.1", b"two")
        self.file("bin/first")
        self.file("bin/second")
        files, links = inventory.walk_tree(str(self.root))
        deps = inventory.resolve_dependency_closure(
            str(self.root), [self.metadata("bin/first", ["libsame.so.1"], "$ORIGIN/../one"),
                             self.metadata("bin/second", ["libsame.so.1"], "$ORIGIN/../two"),
                             self.metadata("one/libsame.so.1"), self.metadata("two/libsame.so.1")],
            {f["path"]: f["sha256"] for f in files}, links, [], [])
        self.assertEqual({d["requester"]: d["resolution"]["file_sha256"] for d in deps},
                         {"bin/first": hashlib.sha256(alpha.read_bytes()).hexdigest(),
                          "bin/second": hashlib.sha256(beta.read_bytes()).hexdigest()})

    def test_rpath_inherits_but_runpath_does_not(self):
        self.file("bin/player")
        self.file("lib/libparent.so.1")
        self.file("lib/libchild.so.1")
        files, links = inventory.walk_tree(str(self.root))
        hashes = {f["path"]: f["sha256"] for f in files}
        for tag in ("rpath", "runpath"):
            first = self.metadata("bin/player", ["libparent.so.1"], **{tag: "$ORIGIN/../lib"})
            second = self.metadata("lib/libparent.so.1", ["libchild.so.1"])
            third = self.metadata("lib/libchild.so.1")
            deps = inventory.resolve_dependency_closure(
                str(self.root), [first, second, third], hashes, links, [], [])
            child_kinds = {d["resolution"]["kind"] for d in deps
                           if d["name"] == "libchild.so.1" and
                           d["requester"] == "lib/libparent.so.1"}
            self.assertEqual(child_kinds, {"missing", "bundled"} if tag == "rpath"
                             else {"missing"})

    def test_explicit_loader_directory_recovers_unsearched_library(self):
        self.file("bin/player")
        self.file("lib/libshared.so.2")
        files, links = inventory.walk_tree(str(self.root))
        deps = inventory.resolve_dependency_closure(
            str(self.root), [self.metadata("bin/player", ["libshared.so.2"]),
                             self.metadata("lib/libshared.so.2")],
            {f["path"]: f["sha256"] for f in files}, links, ["lib"], [])
        self.assertEqual(deps[0]["resolution"]["kind"], "bundled")

    def test_unverified_lib_token_does_not_guess_bundled_path(self):
        self.file("bin/player")
        self.file("lib/libshared.so.2")
        files, links = inventory.walk_tree(str(self.root))
        warnings = []
        deps = inventory.resolve_dependency_closure(
            str(self.root), [self.metadata("bin/player", ["libshared.so.2"],
                                          "$ORIGIN/../$LIB"),
                             self.metadata("lib/libshared.so.2")],
            {f["path"]: f["sha256"] for f in files}, links, [], warnings)
        self.assertEqual(deps[0]["resolution"]["kind"], "missing")
        self.assertTrue(any("loader-specific $LIB" in warning for warning in warnings))

    def test_static_component_mismatch_does_not_claim_ownership(self):
        pc = self.file("lib/pkgconfig/libplacebo.pc",
                       b"Libs.private: /usr/lib/libSPIRV.a\n")
        build = Path(self.tmp.name) / "work/placebo-build/build.ninja"
        build.parent.mkdir(parents=True)
        build.write_text(" LINK_ARGS = -Wl,-soname,libplacebo.so.360 /usr/lib/libSPIRV.a\n")
        warnings = []
        audit = inventory.collect_static_components(str(self.root), str(build), None, warnings)
        self.assertEqual(audit["components"][0]["package"], "glslang-dev")
        self.assertEqual(audit["components"][0]["source"]["source_package"], "glslang")
        pc.write_text("Libs.private: /usr/lib/libSPIRV-Tools.a\n")
        warnings = []
        audit = inventory.collect_static_components(str(self.root), str(build), None, warnings)
        self.assertEqual(audit["status"], "not-audited")
        self.assertEqual(audit["components"], [])
        self.assertTrue(any("mismatch" in warning for warning in warnings))

    def test_pinned_glslang_l_flag_is_static_archive_input(self):
        self.file("lib/pkgconfig/libplacebo.pc",
                  b"Libs.private: -lglslang-default-resource-limits /usr/lib/libSPIRV.a\n")
        build = Path(self.tmp.name) / "work/placebo-build/build.ninja"
        build.parent.mkdir(parents=True)
        build.write_text(
            " LINK_ARGS = -Wl,-soname,libplacebo.so.360 "
            "-lglslang-default-resource-limits /usr/lib/libSPIRV.a\n")
        warnings = []
        audit = inventory.collect_static_components(str(self.root), str(build), None, warnings)
        self.assertIn("libglslang-default-resource-limits.a",
                      audit["components"][0]["archive_names"])
        self.assertEqual(audit["resolved_linker_flags"][0]["owner_package"], "glslang-dev")
        self.assertTrue(audit["resolved_linker_flags"][0]["filelist_url"].startswith("https://packages.ubuntu.com/"))
        self.assertFalse(any("-l flags need" in warning for warning in warnings))

        build.write_text(
            " LINK_ARGS = -Wl,-soname,libplacebo.so.360 "
            "-lglslang-default-resource-limits -lglslang-unpinned /usr/lib/libSPIRV.a\n")
        self.file("lib/pkgconfig/libplacebo.pc",
                  b"Libs.private: -lglslang-default-resource-limits -lglslang-unpinned "
                  b"/usr/lib/libSPIRV.a\n")
        warnings = []
        audit = inventory.collect_static_components(str(self.root), str(build), None, warnings)
        self.assertEqual(audit["unverified_linker_flags"], ["-lglslang-unpinned"])
        self.assertTrue(any("need static/shared linker proof" in warning for warning in warnings))
        build.write_text(
            " LINK_ARGS = -Wl,-soname,libplacebo.so.360 "
            "-L/tmp/alternate -lglslang-default-resource-limits /usr/lib/libSPIRV.a\n")
        self.file("lib/pkgconfig/libplacebo.pc",
                  b"Libs.private: -lglslang-default-resource-limits /usr/lib/libSPIRV.a\n")
        warnings = []
        audit = inventory.collect_static_components(str(self.root), str(build), None, warnings)
        self.assertEqual(audit["unverified_linker_flags"],
                         ["-lglslang-default-resource-limits"])
        self.assertTrue(any("alternate -L paths" in warning for warning in warnings))

    def test_dynamic_glslang_needed_never_counts_as_static_flag_proof(self):
        self.file("lib/libplacebo.so.360", b"\x7fELFsynthetic")
        self.file("lib/pkgconfig/libplacebo.pc",
                  b"Libs.private: -lglslang-default-resource-limits\n")
        build = Path(self.tmp.name) / "work/placebo-build/build.ninja"
        build.parent.mkdir(parents=True)
        build.write_text(
            " LINK_ARGS = -Wl,-soname,libplacebo.so.360 "
            "-lglslang-default-resource-limits\n")
        dynamic = subprocess.CompletedProcess(
            ["readelf"], 0,
            " 0x0000000000000001 (NEEDED) Shared library: [libglslang-default-resource-limits.so.1]\n",
            "")
        warnings = []
        with patch.object(inventory.shutil, "which", return_value="readelf"), \
             patch.object(inventory, "run_cmd", return_value=dynamic):
            audit = inventory.collect_static_components(str(self.root), str(build), None, warnings)
        self.assertEqual(audit["unverified_linker_flags"],
                         ["-lglslang-default-resource-limits"])
        self.assertTrue(any("DT_NEEDED selects" in warning for warning in warnings))

if __name__ == "__main__":
    unittest.main()
