#!/usr/bin/env python3
"""Focused safeguards for immutable inputs, static ELF and two-build comparison."""

import hashlib
import importlib.util
import io
import json
import struct
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

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

    def test_effective_installed_overlay_rejects_old_duplicates_and_extras(self):
        lock = {"base_packages": {"zlib": "1-r0", "musl": "2-r0"},
                "apk_packages": {"zlib": {"version": "1-r1"}, "clang": {"version": "3-r0"}}}
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "installed"
            path.write_text("zlib-1-r1\nmusl-2-r0\nclang-3-r0\n")
            build.check_installed(lock, path)
            for text in ("zlib-1-r0\nmusl-2-r0\nclang-3-r0\n",
                         "zlib-1-r1\nzlib-1-r0\nmusl-2-r0\nclang-3-r0\n",
                         "zlib-1-r1\nmusl-2-r0\nclang-3-r0\nextra-1\n",
                         "zlib-1-r1\nmusl-2-r0\nclang-3-r0\nclang-3-r0\n"):
                path.write_text(text)
                with self.assertRaisesRegex(ValueError, "installed APK closure"):
                    build.check_installed(lock, path)

    def test_forged_signature_member_cannot_authenticate_index(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            index = cache / "indexes/main/x86_64/APKINDEX.tar.gz"
            index.parent.mkdir(parents=True)
            with tarfile.open(index, "w:gz") as archive:
                for name, data in ((".SIGN.RSA.forged", b"not a signature"),
                                   ("APKINDEX", b"P:forged\nV:1\n\n")):
                    member = tarfile.TarInfo(name)
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))
            lock = {"container": "pinned", "platform": "linux/amd64",
                    "base_apk_db_sha256": "a" * 64, "apk_packages": {},
                    "apk_indexes": {"main": {"url": "https://unused.invalid/index",
                                            "sha256": build.sha256(index)}}}
            with patch.object(build.subprocess, "run", return_value=
                              subprocess.CompletedProcess([], 1, b"BAD signature")):
                with self.assertRaisesRegex(ValueError, "index authentication failed"):
                    build.verify_signed_indexes(lock, cache)

    def test_selected_archives_ignore_stale_cache_apks(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            (cache / "apks").mkdir()
            selected = cache / "apks/zlib-1-r1.apk"
            selected.write_bytes(b"selected")
            (cache / "apks/zlib-1-r0.apk").write_bytes(b"stale")
            lock = {"apk_packages": {"zlib": {"archive": selected.name,
                                            "sha256": build.sha256(selected)}}}
            self.assertEqual(build.selected_apks(lock, cache), ["/sources/apks/zlib-1-r1.apk"])
            selected.write_bytes(b"tampered")
            with self.assertRaisesRegex(ValueError, "selected APK"):
                build.selected_apks(lock, cache)

    def test_zlib_source_and_patch_correspondence_fail_closed(self):
        lock = json.loads((HERE / "runtime.lock.json").read_text())
        sources = json.loads((HERE / "alpine-source.lock.json").read_text())
        zlib = sources["linked_sources"]["zlib"]
        for key, changed in (("binary_sha256", "a" * 64), ("version", "1.3.2-r0"),
                             ("aports_commit", "0" * 40)):
            altered = json.loads(json.dumps(sources))
            altered["linked_sources"]["zlib"][key] = changed
            with tempfile.TemporaryDirectory() as temp:
                cache = Path(temp)
                source_lock = cache / "source-lock.json"
                source_lock.write_text(json.dumps(altered))
                changed_lock = json.loads(json.dumps(lock))
                changed_lock["linked_source_closure"]["sha256"] = build.sha256(source_lock)
                with patch.object(build, "SOURCE_LOCK_FILE", source_lock):
                    with self.assertRaisesRegex(ValueError, "zlib: Alpine source does not match"):
                        build.alpine_sources(changed_lock)
        with tempfile.TemporaryDirectory() as temp:
            recipe = Path(temp) / "APKBUILD"
            items = [zlib["upstream_source"], *zlib["aports_inputs"]]
            recipe.write_text('sha512sums="\n' + "".join(
                f"{item['sha512']}  {item['file']}\n" for item in items) + '"\n')
            self.assertEqual(build.verify_alpine_recipe("zlib", zlib, recipe), items)
            altered = json.loads(json.dumps(zlib))
            altered["aports_inputs"] = []
            with self.assertRaisesRegex(ValueError, "aports input list changed"):
                build.verify_alpine_recipe("zlib", altered, recipe)
            altered = json.loads(json.dumps(zlib))
            altered["aports_inputs"][0]["sha512"] = "f" * 128
            with self.assertRaisesRegex(ValueError, "checksum differs"):
                build.verify_alpine_recipe("zlib", altered, recipe)

    def test_missing_private_bootstrap_never_counts_as_complete(self):
        lock = json.loads((HERE / "runtime.lock.json").read_text())
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            with self.assertRaisesRegex(ValueError, "private input inventory"):
                build.verify_private_inputs(lock, cache)
            (cache / "private-inputs.json").write_text(json.dumps({
                "schema": 1, "container": lock["container"],
                "runtime_lock_sha256": build.sha256(HERE / "runtime.lock.json"),
                "files_sha256": {}}))
            with self.assertRaisesRegex(ValueError, "private input inventory"):
                build.verify_private_inputs(lock, cache)

    def test_failed_apk_signature_prevents_identity_acceptance(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            apk = cache / "apks/zlib-1.apk"
            apk.parent.mkdir()
            apk.write_bytes(b"bad signature")
            lock = {"platform": "linux/amd64", "container": "immutable",
                    "base_apk_db_sha256": "a" * 64,
                    "apk_packages": {"zlib": {"archive": apk.name, "sha256": build.sha256(apk)}}}
            with patch.object(build.subprocess, "run", return_value=
                              subprocess.CompletedProcess([], 1, b"BAD signature")):
                with self.assertRaisesRegex(ValueError, "APK authentication failed"):
                    build.verify_selected_apks(lock, cache)

    def test_apk_generated_identity_must_match_authenticated_index(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            apk = cache / "apks/zlib-1.apk"
            apk.parent.mkdir()
            apk.write_bytes(b"archive")
            proof = cache / "authentication"
            proof.mkdir()
            data = b"P:zlib\nV:1\nC:local-control-identity\nL:Zlib\nc:commit\n\n"
            with tarfile.open(proof / "selected-APKINDEX.tar.gz", "w:gz") as archive:
                member = tarfile.TarInfo("APKINDEX")
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
            entry = {"archive": apk.name, "sha256": build.sha256(apk), "version": "1",
                     "index_checksum": "authenticated-upstream-identity", "license": "Zlib",
                     "aports_commit": "commit"}
            lock = {"apk_packages": {"zlib": entry}}
            with patch.object(build, "authenticate"):
                with self.assertRaisesRegex(ValueError, "authenticated APK differs"):
                    build.verify_selected_apks(lock, cache)
                entry["index_checksum"] = "local-control-identity"
                self.assertEqual(build.verify_selected_apks(lock, cache), ["/sources/apks/zlib-1.apk"])

    def test_private_inventory_requires_retained_base_layer_bytes(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            blob_root = cache / "private/base-oci/blobs/sha256"
            blob_root.mkdir(parents=True)
            config, layer = b'{"architecture":"amd64"}', b"exact compressed layer"
            config_sha, layer_sha = (hashlib.sha256(data).hexdigest() for data in (config, layer))
            manifest = json.dumps({"config": {"digest": f"sha256:{config_sha}", "size": len(config)},
                                   "layers": [{"digest": f"sha256:{layer_sha}", "size": len(layer)}]}).encode()
            manifest_sha = hashlib.sha256(manifest).hexdigest()
            for sha, data in ((config_sha, config), (layer_sha, layer), (manifest_sha, manifest)):
                (blob_root / sha).write_bytes(data)
            (cache / "private/base-oci/index.json").write_text(json.dumps({
                "manifests": [{"digest": f"sha256:{manifest_sha}"}]}))
            (cache / "private/base-oci/oci-layout").write_text('{"imageLayoutVersion":"1.0.0"}')
            (cache / "indexes").mkdir()
            (cache / "indexes/repositories").write_text("")
            recipes = cache / "private/recipe"
            recipes.mkdir()
            for name in ("build.py", "container-build.sh", "provision.sh", "relink.sh",
                         "runtime.lock.json", "alpine-source.lock.json", "README.md", "RELINKING.md"):
                (recipes / name).write_bytes((HERE / name).read_bytes())
            lock = {"container": f"alpine@sha256:{manifest_sha}", "apk_indexes": {},
                    "base_packages": {}, "apk_packages": {}, "sources": {}, "notices": {}}
            auth = cache / "private/authentication"
            auth.mkdir()
            (auth / "apk-installed.txt").write_text("")
            (auth / "restored-base-manifest.log").write_bytes(manifest)
            with tarfile.open(auth / "selected-APKINDEX.tar.gz", "w:gz") as archive:
                member = tarfile.TarInfo("APKINDEX")
                member.size = 0
                archive.addfile(member, io.BytesIO(b""))
            for name in ("index", "APK", "offline-restore"):
                (auth / f"{name}.log").write_text("verified")
                (auth / f"{name}.json").write_text(json.dumps({
                    "returncode": 0, "container": lock["container"],
                    "log_sha256": build.sha256(auth / f"{name}.log"),
                    "input_binding": build.private_proof_binding(lock),
                    "output_sha256": ({"selected-APKINDEX.tar.gz":
                                       build.sha256(auth / "selected-APKINDEX.tar.gz")}
                                      if name == "APK" else
                                      {"apk-installed.txt": build.sha256(auth / "apk-installed.txt"),
                                       "restored-base-manifest.log":
                                       build.sha256(auth / "restored-base-manifest.log")}
                                      if name == "offline-restore" else {})}))
            inventory = {"schema": 1, "container": lock["container"],
                         "runtime_lock_sha256": build.sha256(HERE / "runtime.lock.json"),
                         "source_lock_sha256": build.sha256(HERE / "alpine-source.lock.json"),
                         "files_sha256": {path.relative_to(cache).as_posix(): build.sha256(path)
                                          for path in cache.rglob("*") if path.is_file()}}
            (cache / "private-inputs.json").write_text(json.dumps(inventory))
            with patch.object(build, "alpine_sources", return_value={}):
                self.assertEqual(build.verify_private_inputs(lock, cache), inventory)
                for name in ("selected-APKINDEX.tar.gz", "apk-installed.txt"):
                    relative = f"private/authentication/{name}"
                    saved = inventory["files_sha256"].pop(relative)
                    (cache / "private-inputs.json").write_text(json.dumps(inventory))
                    with self.assertRaisesRegex(ValueError, "omits required inputs"):
                        build.verify_private_inputs(lock, cache)
                    inventory["files_sha256"][relative] = saved
                (cache / "private-inputs.json").write_text(json.dumps(inventory))
                changed_lock = {**lock, "source_date_epoch": 1}
                with self.assertRaisesRegex(ValueError, "lacks successful index proof"):
                    build.verify_private_inputs(changed_lock, cache)
                receipt_path = auth / "index.json"
                receipt = json.loads(receipt_path.read_text())
                receipt["input_binding"]["source_lock_sha256"] = "0" * 64
                receipt_path.write_text(json.dumps(receipt))
                inventory["files_sha256"]["private/authentication/index.json"] = build.sha256(receipt_path)
                (cache / "private-inputs.json").write_text(json.dumps(inventory))
                with self.assertRaisesRegex(ValueError, "lacks successful index proof"):
                    build.verify_private_inputs(lock, cache)
                (blob_root / layer_sha).unlink()
                with self.assertRaisesRegex(ValueError, "missing or changed file"):
                    build.verify_private_inputs(lock, cache)

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
