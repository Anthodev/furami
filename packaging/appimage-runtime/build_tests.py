#!/usr/bin/env python3
"""Focused safeguards for immutable inputs, static ELF and two-build comparison."""

import hashlib
import importlib.util
import io
import json
import os
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


def write_apk_index(path, data):
    with tarfile.open(path, "w:gz") as archive:
        member = tarfile.TarInfo("APKINDEX")
        member.size = len(data)
        archive.addfile(member, io.BytesIO(data))


class RuntimeRecipeTests(unittest.TestCase):
    def test_changed_apk_closure_fails_before_container_start(self):
        lock = json.loads((HERE / "runtime.lock.json").read_text())
        lock["apk_packages"]["zstd-static"]["sha256"] = "not-a-digest"
        with self.assertRaisesRegex(ValueError, "missing repository or archive SHA256"):
            build.verify_lock(lock)

    def test_normal_build_acquires_only_exact_archives_before_offline_runtime_container(self):
        lock = json.loads((HERE / "runtime.lock.json").read_text())
        packages = lock["apk_packages"]
        selected = [f"/selected-apks/{entry['archive']}" for _, entry in sorted(packages.items())]
        with tempfile.TemporaryDirectory() as temp:
            cache, output = Path(temp) / "sources", Path(temp) / "runtime"
            with (patch.object(build, "verify_alpine_sources", return_value=({}, 0)),
                  patch.object(build, "obtain") as obtain,
                  patch.object(build, "verify_selected_apks", return_value=selected),
                  patch.object(build.subprocess, "run", return_value=
                               subprocess.CompletedProcess([], 1)) as run):
                with self.assertRaisesRegex(RuntimeError, "runtime container exited 1"):
                    build.build(output, cache)
            self.assertCountEqual(
                [call.args for call in obtain.call_args_list],
                [(entry["url"], entry["sha256"], cache / entry["archive"])
                 for entry in (*lock["sources"].values(), *lock["notices"].values())]
                + [(f"{lock['repository_base_url']}/{entry['repository']}/x86_64/{entry['archive']}",
                    entry["sha256"], cache / "apks" / entry["archive"]) for entry in packages.values()])
            command, = run.call_args.args
            self.assertIn("--pull=missing", command)
            self.assertIn("--network=none", command)
            self.assertEqual(command[-len(selected):], selected)
            self.assertFalse((cache / "indexes").exists())

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

    def test_selected_snapshot_ignores_stale_apks_and_is_independent_of_cache(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            (cache / "apks").mkdir()
            selected = cache / "apks/zlib-1-r1.apk"
            selected.write_bytes(b"selected")
            (cache / "apks/zlib-1-r0.apk").write_bytes(b"stale")
            checksum = build.sha256(selected)
            lock = {"apk_packages": {"zlib": {"archive": selected.name, "sha256": checksum}}}
            snapshot = cache / "snapshot"
            snapshot.mkdir()
            self.assertEqual(build.stage_selected_apks(lock, cache, snapshot), ["/selected-apks/zlib-1-r1.apk"])
            selected.write_bytes(b"tampered")
            self.assertEqual((snapshot / selected.name).read_bytes(), b"selected")
            self.assertEqual((snapshot / "SHA256SUMS").read_text(), f"{checksum}  /selected-apks/{selected.name}\n")
            self.assertEqual({path.name for path in snapshot.iterdir()}, {selected.name, "SHA256SUMS"})

    def test_provision_checks_locked_bytes_before_invoking_apk(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            snapshot = root / "snapshot"
            snapshot.mkdir()
            apk = snapshot / "zlib-1.apk"
            apk.write_bytes(b"locked package")
            unlisted = snapshot / "extra-1.apk"
            unlisted.write_bytes(b"unlisted package")
            (snapshot / "SHA256SUMS").write_text(f"{build.sha256(apk)}  {apk}\n")
            database = root / "installed"
            database.write_bytes(b"base database")
            # Execute the production script without Podman. Remap only its
            # container paths and observe which commands would reach apk.
            provision = (HERE / "provision.sh").read_text()
            script = root / "provision.sh"
            script.write_text(provision.replace("/selected-apks", str(snapshot))
                              .replace("/lib/apk/db/installed", str(database)))
            fake_apk = root / "apk"
            fake_apk.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$APK_LOG"\n')
            fake_apk.chmod(0o755)
            log = root / "apk.log"
            env = {**os.environ, "BASE_APK_DB_SHA256": build.sha256(database),
                   "PATH": f"{root}:{os.environ['PATH']}", "APK_LOG": str(log)}

            def run(*archives):
                return subprocess.run(["sh", script, *archives], env=env, text=True, capture_output=True)

            result = run(apk)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(log.read_text().splitlines(), [
                f"--no-network --repositories-file /dev/null verify {apk}",
                f"--no-network --repositories-file /dev/null add --no-cache {apk}"])
            log.unlink()
            self.assertNotEqual(run(apk, unlisted).returncode, 0)
            self.assertFalse(log.exists())
            apk.write_bytes(b"wrong bytes, same package filename")
            self.assertNotEqual(run(apk).returncode, 0)
            self.assertFalse(log.exists())

    def test_linked_source_binary_identity_and_apkbuild_inputs_fail_closed(self):
        lock = json.loads((HERE / "runtime.lock.json").read_text())
        sources = json.loads((HERE / "alpine-source.lock.json").read_text())
        for key, changed in (("binary_sha256", "a" * 64), ("version", "0-r0"),
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
        for name in ("zlib", "musl"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temp:
                source = sources["linked_sources"][name]
                recipe = Path(temp) / "APKBUILD"
                items = [source["upstream_source"], *source["aports_inputs"]]
                recipe.write_text('sha512sums="\n' + "".join(
                    f"{item['sha512']}  {item['file']}\n" for item in items) + '"\n')
                self.assertEqual(build.verify_alpine_recipe(name, source, recipe), items)
                if name == "musl":
                    altered = json.loads(json.dumps(source))
                    altered["aports_inputs"].pop()
                    with self.assertRaisesRegex(ValueError, "aports input list changed"):
                        build.verify_alpine_recipe(name, altered, recipe)
                    altered = json.loads(json.dumps(source))
                    altered["aports_inputs"][0]["sha512"] = "f" * 128
                    with self.assertRaisesRegex(ValueError, "checksum differs"):
                        build.verify_alpine_recipe(name, altered, recipe)

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

    def test_forged_signature_member_cannot_authenticate_apk(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            apk = cache / "apks/zlib-1.apk"
            apk.parent.mkdir()
            snapshot = cache / "snapshot"
            snapshot.mkdir()
            with tarfile.open(apk, "w:gz") as archive:
                for name, data in ((".SIGN.RSA.forged", b"not a signature"),
                                   (".PKGINFO", b"pkgname = zlib\npkgver = 1\n")):
                    member = tarfile.TarInfo(name)
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))
            lock = {"platform": "linux/amd64", "container": "immutable",
                    "base_apk_db_sha256": "a" * 64,
                    "apk_packages": {"zlib": {"archive": apk.name, "sha256": build.sha256(apk)}}}
            with (patch.object(build.subprocess, "run", return_value=
                               subprocess.CompletedProcess([], 1, b"BAD signature")),
                  patch.object(build, "check_selected_apk_metadata") as metadata):
                with self.assertRaisesRegex(ValueError, "APK authentication failed"):
                    build.verify_selected_apks(lock, cache, snapshot)
                metadata.assert_not_called()

    def test_verification_needs_no_repository_index_and_rejects_metadata_mismatch(self):
        with tempfile.TemporaryDirectory() as temp:
            cache = Path(temp)
            apk = cache / "apks/zlib-1.apk"
            apk.parent.mkdir()
            apk.write_bytes(b"archive")
            snapshot = cache / "snapshot"
            snapshot.mkdir()
            selected = ["/selected-apks/zlib-1.apk"]
            proofs = []
            data = b"P:zlib\nV:1\nA:x86_64\nC:control-identity\nL:Zlib\nc:commit\n\n"

            def run(command, **kwargs):
                self.assertIn("--network=none", command)
                self.assertEqual(command[-1:], selected)
                self.assertIn(f"{snapshot}:/selected-apks:ro", command)
                self.assertEqual(command[-3:-1], ["/recipe/provision.sh", "--index"])
                proof = next(Path(arg.removesuffix(":/proof:rw")) for arg in command if arg.endswith(":/proof:rw"))
                proofs.append(proof)
                write_apk_index(proof / "selected-APKINDEX.tar.gz", data)
                return subprocess.CompletedProcess(command, 0, b"OK")

            entry = {"archive": apk.name, "sha256": build.sha256(apk), "version": "1",
                     "index_checksum": "control-identity", "license": "Zlib",
                     "aports_commit": "commit"}
            lock = {"platform": "linux/amd64", "container": "immutable",
                    "base_apk_db_sha256": "a" * 64, "apk_packages": {"zlib": entry}}
            with (patch.object(build.urllib.request, "urlopen") as network,
                  patch.object(build.subprocess, "run", side_effect=run)):
                self.assertEqual(build.verify_selected_apks(lock, cache, snapshot), selected)
                for field, changed in (("version", "2"), ("index_checksum", "other"),
                                       ("license", "MIT"), ("aports_commit", "other")):
                    with self.subTest(field=field):
                        actual = lock["apk_packages"]["zlib"]
                        lock["apk_packages"]["zlib"] = {**actual, field: changed}
                        with self.assertRaisesRegex(ValueError, "authenticated (selected )?APK"):
                            build.verify_selected_apks(lock, cache, snapshot)
                        lock["apk_packages"]["zlib"] = actual
                data = data.replace(b"x86_64", b"aarch64")
                with self.assertRaisesRegex(ValueError, "authenticated APK metadata differs"):
                    build.verify_selected_apks(lock, cache, snapshot)
                network.assert_not_called()
            self.assertFalse((cache / "indexes").exists())
            self.assertEqual(len(proofs), len(set(proofs)))
            with tarfile.open(proofs[0] / "selected-APKINDEX.tar.gz") as proof:
                self.assertIn(b"A:x86_64\n", proof.extractfile("APKINDEX").read())

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
            recipes = cache / "private/recipe"
            recipes.mkdir()
            for name in ("build.py", "container-build.sh", "provision.sh", "relink.sh",
                         "runtime.lock.json", "alpine-source.lock.json", "README.md", "RELINKING.md"):
                (recipes / name).write_bytes((HERE / name).read_bytes())
            lock = {"container": f"alpine@sha256:{manifest_sha}",
                    "base_packages": {}, "apk_packages": {}, "sources": {}, "notices": {}}
            auth = cache / "private/authentication"
            auth.mkdir()
            (auth / "apk-installed.txt").write_text("")
            (auth / "restored-base-manifest.log").write_bytes(manifest)
            write_apk_index(auth / "selected-APKINDEX.tar.gz", b"")
            for name in ("APK", "offline-restore"):
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
                                           build.sha256(auth / "restored-base-manifest.log")})}))
            inventory = {"schema": 1, "container": lock["container"],
                         "runtime_lock_sha256": build.sha256(HERE / "runtime.lock.json"),
                         "source_lock_sha256": build.sha256(HERE / "alpine-source.lock.json"),
                         "files_sha256": {path.relative_to(cache).as_posix(): build.sha256(path)
                                          for path in cache.rglob("*") if path.is_file()}}
            (cache / "private-inputs.json").write_text(json.dumps(inventory))
            with patch.object(build, "alpine_sources", return_value={}):
                self.assertEqual(build.verify_private_inputs(lock, cache), inventory)
                for name in ("selected-APKINDEX.tar.gz", "apk-installed.txt", "offline-restore.json"):
                    relative = f"private/authentication/{name}"
                    saved = inventory["files_sha256"].pop(relative)
                    (cache / "private-inputs.json").write_text(json.dumps(inventory))
                    with self.assertRaisesRegex(ValueError, "omits required inputs"):
                        build.verify_private_inputs(lock, cache)
                    inventory["files_sha256"][relative] = saved
                (cache / "private-inputs.json").write_text(json.dumps(inventory))
                changed_lock = {**lock, "source_date_epoch": 1}
                with self.assertRaisesRegex(ValueError, "lacks successful APK proof"):
                    build.verify_private_inputs(changed_lock, cache)
                receipt_path = auth / "APK.json"
                receipt = json.loads(receipt_path.read_text())
                receipt["input_binding"]["source_lock_sha256"] = "0" * 64
                receipt_path.write_text(json.dumps(receipt))
                inventory["files_sha256"]["private/authentication/APK.json"] = build.sha256(receipt_path)
                (cache / "private-inputs.json").write_text(json.dumps(inventory))
                with self.assertRaisesRegex(ValueError, "lacks successful APK proof"):
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
