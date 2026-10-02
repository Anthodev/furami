#!/usr/bin/env python3
"""Build the type-2 x86_64 runtime from SHA256-locked sources and APKs."""

import argparse
import hashlib
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
LOCK_FILE = HERE / "runtime.lock.json"
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
COMMIT = "75849dce7cc37e4319b633df1f116ca895c71a12"
COMPONENTS = {
    "type2-runtime": ("MIT", "type2_runtime", "type2-runtime.LICENSE"),
    "libfuse3": ("LGPL-2.1-only", "libfuse", "libfuse.LGPL2.txt"),
    "squashfuse": ("BSD-2-Clause", "squashfuse", "squashfuse.LICENSE"),
    "musl": ("MIT", "musl", "musl.COPYRIGHT"),
    "zstd": ("BSD-3-Clause", "zstd", "zstd.LICENSE"),
    "zlib": ("Zlib", "zlib", "zlib.LICENSE"),
    "mimalloc": ("MIT", "mimalloc", "mimalloc.LICENSE"),
}
STATIC_APK = {"musl": "musl-dev", "zstd": "zstd-static",
              "zlib": "zlib-static", "mimalloc": "mimalloc2-dev"}
LINKED_APK_SOURCES = {**STATIC_APK, "gcc-startup": "gcc"}
SOURCE_LOCK_FILE = HERE / "alpine-source.lock.json"
ARCHIVE_COMPONENT = {
    "libsquashfuse.a": "squashfuse",
    "libsquashfuse_ll.a": "squashfuse",
    "libfuse3.a": "libfuse3",
    "libzstd.a": "zstd",
    "libz.a": "zlib",
    "libmimalloc.a": "mimalloc",
    "libmimalloc-secure.a": "mimalloc",
    "libc.a": "musl",
    "libm.a": "musl",
    "libpthread.a": "musl",
    "libdl.a": "musl",
    "librt.a": "musl",
    "libutil.a": "musl",
    "libxnet.a": "musl",
    "libgcc.a": "gcc-runtime",
    "libgcc_eh.a": "gcc-runtime",
    "libatomic.a": "gcc-runtime",
}


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify_lock(lock):
    if lock.get("schema") != 1 or lock.get("platform") != "linux/amd64":
        raise ValueError("unsupported runtime lock schema or platform")
    if not re.fullmatch(r"docker.io/library/alpine@sha256:[0-9a-f]{64}", lock.get("container", "")):
        raise ValueError("Alpine image must be pinned by its linux/amd64 OCI manifest digest")
    if lock["sources"]["type2_runtime"]["version"] != COMMIT:
        raise ValueError("unexpected type2-runtime commit")
    if len(lock.get("apk_packages", {})) != 135 or set(lock["apk_indexes"]) != {"main", "community"}:
        raise ValueError("incomplete frozen APK closure")
    if not isinstance(lock.get("source_date_epoch"), int) or not SHA256.fullmatch(lock.get("base_apk_db_sha256", "")):
        raise ValueError("missing source epoch or base image APK database digest")
    source_lock = lock.get("linked_source_closure", {})
    if source_lock.get("file") != SOURCE_LOCK_FILE.name or not SHA256.fullmatch(source_lock.get("sha256", "")):
        raise ValueError("missing pinned linked Alpine source closure")
    for name, entry in {**lock["sources"], **lock["notices"], **lock["apk_indexes"]}.items():
        if not entry["url"].startswith("https://") or not SHA256.fullmatch(entry["sha256"]):
            raise ValueError(f"{name}: source URL/SHA256 not pinned")
    for name, entry in lock["apk_packages"].items():
        if not re.fullmatch(r"[a-z0-9][a-z0-9+_.-]*", name):
            raise ValueError(f"invalid APK name {name}")
        if entry["repository"] not in lock["apk_indexes"] or not SHA256.fullmatch(entry["sha256"]):
            raise ValueError(f"{name}: missing repository or archive SHA256")
        if entry["archive"] != f"{name}-{entry['version']}.apk":
            raise ValueError(f"{name}: inconsistent archive name")
        if not re.fullmatch(r"Q1[A-Za-z0-9+/]{27}=", entry["index_checksum"]):
            raise ValueError(f"{name}: missing signed index checksum")
    for entry in lock["sources"].values():
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", entry["archive"]):
            raise ValueError("unsafe source archive name")


def obtain(url, expected, path):
    if path.is_symlink():
        raise ValueError(f"refusing symlink in source cache: {path}")
    if path.is_file():
        actual = sha256(path)
        if actual != expected:
            raise ValueError(f"cached {path.name}: SHA256 {actual} != {expected}")
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".partial")
    try:
        digest = hashlib.sha256()
        with urllib.request.urlopen(url, timeout=120) as response, tmp.open("wb") as target:
            for block in iter(lambda: response.read(1024 * 1024), b""):
                digest.update(block)
                target.write(block)
        if digest.hexdigest() != expected:
            raise ValueError(f"downloaded {path.name}: SHA256 {digest.hexdigest()} != {expected}")
        tmp.replace(path)
    finally:
        tmp.unlink(missing_ok=True)


def verify_signed_indexes(lock, cache):
    """Bind each APK to frozen signed index metadata before fetching archives."""
    indexed = {}
    for repo, entry in lock["apk_indexes"].items():
        index_path = cache / "indexes" / repo / "x86_64" / "APKINDEX.tar.gz"
        obtain(entry["url"], entry["sha256"], index_path)
        with tarfile.open(index_path, "r:gz") as archive:
            if not any(member.name.startswith(".SIGN.RSA.") for member in archive.getmembers()):
                raise ValueError(f"{repo}: signed APKINDEX missing signature")
            text = archive.extractfile("APKINDEX").read().decode("utf-8")
        for stanza in text.strip().split("\n\n"):
            fields = dict(line.split(":", 1) for line in stanza.splitlines() if ":" in line)
            if "P" in fields:
                indexed[(repo, fields["P"], fields["V"])] = fields
    for name, entry in lock["apk_packages"].items():
        fields = indexed.get((entry["repository"], name, entry["version"]))
        if fields is None or any(fields.get(field) != entry[lock_field] for field, lock_field in
                                 (("C", "index_checksum"), ("L", "license"), ("c", "aports_commit"))):
            raise ValueError(f"{name}: package metadata differs from frozen signed index")
    (cache / "indexes" / "repositories").write_text(
        "file:///sources/indexes/main\nfile:///sources/indexes/community\n"
    )


def verify_alpine_sources(lock, cache):
    if sha256(SOURCE_LOCK_FILE) != lock["linked_source_closure"]["sha256"]:
        raise ValueError("linked Alpine source closure changed")
    sources = json.loads(SOURCE_LOCK_FILE.read_text())["linked_sources"]
    if set(sources) != set(LINKED_APK_SOURCES):
        raise ValueError("incomplete linked Alpine source closure")
    downloads = []
    for name, source in sources.items():
        package = lock["apk_packages"][LINKED_APK_SOURCES[name]]
        if (source["binary_apk"] != LINKED_APK_SOURCES[name]
                or source["binary_sha256"] != package["sha256"]
                or source["version"] != package["version"]
                or source["aports_commit"] != package["aports_commit"]
                or source["aports_commit"] not in source["apkbuild"]["url"]):
            raise ValueError(f"{name}: Alpine source does not match signed binary package")
        source_dir = cache / "alpine-sources" / name
        recipe = source["apkbuild"]
        obtain(recipe["url"], recipe["sha256"], source_dir / "APKBUILD")
        text = (source_dir / "APKBUILD").read_text()
        sums = re.search(r'^sha512sums="(.*?)^"', text, re.MULTILINE | re.DOTALL)
        if not sums:
            raise ValueError(f"{name}: missing aports source checksums")
        listed = dict(re.findall(r'^([0-9a-f]{128})\s+(\S+)$', sums.group(1), re.MULTILINE))
        declared = [source["upstream_source"], *source["aports_inputs"]]
        if {item["file"] for item in declared} != set(listed.values()):
            raise ValueError(f"{name}: aports input list changed")
        for item in declared:
            if (not re.fullmatch(r"[a-zA-Z0-9_.-]+", item["file"])
                    or item["sha512"] not in listed
                    or listed[item["sha512"]] != item["file"]
                    or not SHA256.fullmatch(item["sha256"])):
                raise ValueError(f"{name}: source or patch checksum differs from APKBUILD")
            downloads.append((item, source_dir / item["file"]))

    def verify_input(row):
        item, path = row
        obtain(item["url"], item["sha256"], path)
        with path.open("rb") as stream:
            if hashlib.file_digest(stream, "sha512").hexdigest() != item["sha512"]:
                raise ValueError(f"Alpine source SHA512 mismatch: {path}")

    with ThreadPoolExecutor(max_workers=8) as workers:
        list(workers.map(verify_input, downloads))
    return sources, len(downloads)


def check_elf(path):
    data = path.read_bytes()
    if (len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01"
            or data[8:11] != b"AI\x02"):
        raise ValueError("runtime lacks x86_64 ELF header or AppImage type-2 magic")
    elf_type, machine = struct.unpack_from("<HH", data, 16)
    if elf_type != 3 or machine != 62:
        raise ValueError("runtime is not x86_64 static PIE (ET_DYN)")
    ph_offset = struct.unpack_from("<Q", data, 32)[0]
    ph_size, ph_count = struct.unpack_from("<HH", data, 54)
    if ph_size < 56 or ph_offset + ph_size * ph_count > len(data):
        raise ValueError("invalid runtime ELF program headers")
    load_count = 0
    for n in range(ph_count):
        offset = ph_offset + n * ph_size
        kind = struct.unpack_from("<I", data, offset)[0]
        if kind == 3:
            raise ValueError("runtime has PT_INTERP; static ELF required")
        if kind == 1:
            load_count += 1
        if kind == 2:
            dyn_offset = struct.unpack_from("<Q", data, offset + 8)[0]
            dyn_size = struct.unpack_from("<Q", data, offset + 32)[0]
            if dyn_size % 16 or dyn_offset + dyn_size > len(data):
                raise ValueError("invalid ELF dynamic segment")
            for entry in range(dyn_offset, dyn_offset + dyn_size, 16):
                tag = struct.unpack_from("<Q", data, entry)[0]
                if tag == 1:
                    raise ValueError("runtime has DT_NEEDED; static ELF required")
                if tag == 0:
                    break
    if not load_count:
        raise ValueError("runtime has no PT_LOAD")


def linked_archives(map_path):
    text = map_path.read_text(errors="replace")
    archive_paths = sorted(set(re.findall(r"(/[^\s():]+\.a)\([^)]*\)", text)))
    if not archive_paths:
        raise ValueError("link map contains no static archive members")
    result = {}
    for archive in archive_paths:
        basename = Path(archive).name
        component = ARCHIVE_COMPONENT.get(basename)
        if not component:
            raise ValueError(f"unclassified static archive in runtime: {archive}")
        result[archive] = component
    required = {"squashfuse", "libfuse3", "zstd", "zlib", "mimalloc", "musl"}
    absent = required - set(result.values())
    if absent:
        raise ValueError(f"link map misses expected static components: {sorted(absent)}")
    return result


def linked_objects(map_path, lock):
    """Attribute every directly linked .o, including GCC and musl CRT code."""
    text = map_path.read_text()
    if "Linker script and memory map" not in text:
        raise ValueError("link map lacks output section inventory")
    output_sections = text.split("Linker script and memory map", 1)[1]
    loaded = re.findall(r"^LOAD (/\S+\.o)$", output_sections, re.MULTILINE)
    gcc_version = lock["apk_packages"]["gcc"]["version"].split("-r", 1)[0]
    gcc_dir = f"/usr/lib/gcc/x86_64-alpine-linux-musl/{gcc_version}"
    expected = {f"/usr/lib/{name}.o": ("musl", "musl-dev")
                for name in ("rcrt1", "crti", "crtn")}
    expected.update({f"{gcc_dir}/{name}.o": ("gcc-startup", "gcc")
                     for name in ("crtbeginS", "crtendS")})
    temporary = [path for path in loaded if path not in expected]
    if (len(loaded) != len(expected) + 1 or len(temporary) != 1
            or not re.fullmatch(r"/tmp/runtime-[0-9a-f]{6}\.o", temporary[0])
            or len(set(loaded)) != len(loaded) or set(loaded) - set(temporary) != set(expected)):
        raise ValueError(f"unexpected directly linked objects: {loaded}")
    result = {}
    for path in loaded:
        retained = []
        for line in output_sections.splitlines():
            match = re.match(r"^\s+(\.[^\s]+)\s+0x[0-9a-fA-F]+\s+0x([0-9a-fA-F]+)\s+"
                             + re.escape(path) + r"$", line)
            if match and int(match.group(2), 16):
                retained.append(match.group(1))
        if not retained:
            raise ValueError(f"direct object has no retained section: {path}")
        if path in expected:
            component, package_name = expected[path]
            package = lock["apk_packages"][package_name]
            result[path] = {"component": component, "binary_apk": package_name,
                            "binary_version": package["version"], "binary_sha256": package["sha256"],
                            "retained_sections": sorted(set(retained))}
        else:
            result["runtime.c"] = {"component": "type2-runtime", "source": "type2_runtime",
                                   "source_sha256": lock["sources"]["type2_runtime"]["sha256"],
                                   "retained_sections": sorted(set(retained))}
    return result


def check_installed(lock, path):
    actual = set(path.read_text().splitlines())
    expected = {f"{name}-{version}" for name, version in lock["base_packages"].items()}
    expected.update(f"{name}-{entry['version']}" for name, entry in lock["apk_packages"].items())
    if actual != expected:
        raise ValueError(f"installed APK closure differs: missing {sorted(expected - actual)}, extra {sorted(actual - expected)}")


def artifact_hashes(output):
    """Hash declared deliverables only, never container-owned work directories."""
    names = ("runtime-x86_64", "runtime-version.txt", "link.map", "apk-installed.txt",
             "clang-version.txt", "ld-version.txt", "meson-version.txt", "ninja-version.txt")
    records = {name: sha256(output / name) for name in names}
    for folder in ("licenses", "relink"):
        for path in sorted((output / folder).iterdir()):
            if not path.is_file() or path.is_symlink():
                raise ValueError(f"unexpected {folder} entry: {path}")
            records[f"{folder}/{path.name}"] = sha256(path)
    return records


def build(output, cache, modified_libfuse=None):
    lock = json.loads(LOCK_FILE.read_text())
    verify_lock(lock)
    if not output.is_absolute() or output.exists():
        raise ValueError("output must be a new absolute directory")
    if cache == output or output in cache.parents:
        raise ValueError("source cache must be outside the new output directory")
    modified_hash = None
    if modified_libfuse is not None:
        if not modified_libfuse.is_absolute() or not modified_libfuse.is_file() or modified_libfuse.is_symlink():
            raise ValueError("modified libfuse source must be a regular absolute archive")
        modified_hash = sha256(modified_libfuse)
    cache.mkdir(parents=True, exist_ok=True)
    verify_signed_indexes(lock, cache)
    if sha256(SOURCE_LOCK_FILE) != lock["linked_source_closure"]["sha256"]:
        raise ValueError("linked Alpine source lock differs from canonical pin")
    if modified_libfuse is None:
        alpine_sources, source_count = verify_alpine_sources(lock, cache)
    else:
        # Relink changes libfuse only. Existing APK static objects are verified;
        # permissive/exception source inputs are not required to compile them.
        alpine_sources = json.loads(SOURCE_LOCK_FILE.read_text())["linked_sources"]
        source_count = 0
    downloads = [(entry["url"], entry["sha256"], cache / entry["archive"])
                 for entry in (*lock["sources"].values(), *lock["notices"].values())]
    downloads += [(f"{lock['repository_base_url']}/{entry['repository']}/x86_64/{entry['archive']}",
                   entry["sha256"], cache / "apks" / entry["archive"])
                  for entry in lock["apk_packages"].values()]
    with ThreadPoolExecutor(max_workers=8) as workers:
        list(workers.map(lambda download: obtain(*download), downloads))
    output.mkdir(parents=True)
    cmd = ["podman", "run", "--rm", "--pull=always", "--platform", lock["platform"],
           "--network=none", "--security-opt", "label=disable", "--volume", f"{cache}:/sources:ro",
           "--volume", f"{HERE}:/recipe:ro", "--volume", f"{output}:/out:rw",
           "--env", f"BASE_APK_DB_SHA256={lock['base_apk_db_sha256']}",
           "--env", f"RUNTIME_COMMIT={COMMIT}", "--env", f"SOURCE_DATE_EPOCH={lock['source_date_epoch']}",
           "--env", f"FURAMI_BUILD_JOBS={os.environ.get('FURAMI_BUILD_JOBS', '2')}",
           *([] if modified_hash is None else ["--volume", f"{modified_libfuse}:/modified-fuse.tar.xz:ro",
                                                 "--env", "FUSE_ARCHIVE=/modified-fuse.tar.xz"]),
           lock["container"], "sh", "/recipe/container-build.sh"]
    with (output / "build.log").open("w") as log:
        outcome = subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT, check=False)
    if outcome.returncode:
        raise RuntimeError(f"runtime container exited {outcome.returncode}; see {output / 'build.log'}")
    artifact = output / "runtime-x86_64"
    check_elf(artifact)
    version = (output / "runtime-version.txt").read_text()
    if f"AppImage runtime version: {COMMIT}" not in version:
        raise ValueError(f"runtime version probe mismatches source commit: {version}")
    check_installed(lock, output / "apk-installed.txt")
    archive_map = linked_archives(output / "link.map")
    object_map = linked_objects(output / "link.map", lock)
    for name, entry in lock["notices"].items():
        if name != "llvm":  # LLVM toolchain is build-only, not static runtime code.
            shutil.copyfile(cache / entry["archive"], output / "licenses" / entry["archive"])

    relink = output / "relink"
    relink.mkdir()
    for entry in lock["sources"].values():
        shutil.copyfile(cache / entry["archive"], relink / entry["archive"])
    for name in ("build.py", "container-build.sh", "relink.sh", "RELINKING.md",
                 "runtime.lock.json", "alpine-source.lock.json"):
        shutil.copyfile(HERE / name, relink / name)
    (relink / "relink.sh").chmod(0o755)
    if modified_hash is not None:
        shutil.copyfile(modified_libfuse, relink / "modified-libfuse.tar.xz")

    components = []
    for name, (spdx, source, notice) in COMPONENTS.items():
        notices = [f"licenses/{notice}"]
        if name == "type2-runtime":
            notices.append("licenses/type2-runtime.runtime.c.NOTICE")
        elif name == "libfuse3":
            notices.append("licenses/libfuse.LICENSE")
        if source in lock["sources"]:
            entry = lock["sources"][source]
            source_url, source_hash = entry["url"], entry["sha256"]
            binary_input = None
            if name == "libfuse3" and modified_hash is not None:
                source_url, source_hash = "bundled:relink/modified-libfuse.tar.xz", modified_hash
        else:
            entry = alpine_sources[source]["upstream_source"]
            source_url, source_hash = entry["url"], entry["sha256"]
            package_name = STATIC_APK[source]
            package = lock["apk_packages"][package_name]
            binary_input = {"package": package_name, "version": package["version"],
                            "sha256": package["sha256"], "aports_commit": package["aports_commit"],
                            "source_lock_key": source}
        component = {"name": name, "spdx": spdx, "source_url": source_url,
                     "source_sha256": source_hash, "license_file": notices[0],
                     "license_sha256": sha256(output / notices[0]),
                     "notice_files": notices}
        if binary_input is not None:
            component["binary_input"] = binary_input
        components.append(component)

    gcc_source = alpine_sources["gcc-startup"]["upstream_source"]
    gcc_package = lock["apk_packages"]["gcc"]
    gcc_notices = ["licenses/gcc.COPYING3", "licenses/gcc.COPYING.RUNTIME"]
    gcc_component = {"name": "gcc-startup", "spdx": "GPL-3.0-or-later WITH GCC-exception-3.1",
                     "source_url": gcc_source["url"], "source_sha256": gcc_source["sha256"],
                     "binary_input": {"package": "gcc", "version": gcc_package["version"],
                                      "sha256": gcc_package["sha256"],
                                      "aports_commit": gcc_package["aports_commit"],
                                      "source_lock_key": "gcc-startup"},
                     "license_file": gcc_notices[0], "exception_file": gcc_notices[1],
                     "notice_files": gcc_notices}
    components.append(gcc_component)
    if "gcc-runtime" in archive_map.values():
        components.append({**gcc_component, "name": "gcc-runtime"})
    # Container removed extracted sources inside its user namespace. Never
    # recurse into container-owned work trees from the rootless host.
    if (output / "work").exists():
        raise ValueError("container left source work tree behind")
    records = artifact_hashes(output)
    manifest = {"schema": 1, "artifact": {"path": "runtime-x86_64", "sha256": records["runtime-x86_64"],
                                           "elf_machine": "EM_X86_64", "elf_type": "ET_DYN", "static": True},
                "lock_sha256": sha256(LOCK_FILE), "container": lock["container"],
                "source_date_epoch": lock["source_date_epoch"], "sources": lock["sources"],
                "apk_closure": {"base_apk_db_sha256": lock["base_apk_db_sha256"],
                                "indexes": lock["apk_indexes"], "base_packages": lock["base_packages"],
                                "direct_packages": lock["direct_apk"], "packages": lock["apk_packages"],
                                "installed_sha256": records["apk-installed.txt"]},
                "toolchain": {key: records[key] for key in
                              ("clang-version.txt", "ld-version.txt", "meson-version.txt", "ninja-version.txt")},
                "build_flags": {"libfuse": "meson --prefix=/usr --libdir=lib --default-library=static -Dutils=false -Dexamples=false -Dtests=false -Duseroot=false",
                                "squashfuse": "CFLAGS=-ffunction-sections -fdata-sections -Os -ffile-prefix-map=/out/work=/usr/src/type2-runtime; ./configure --disable-shared --enable-static --disable-demo LDFLAGS=-static",
                                "runtime": "-std=gnu99 -Os -D_FILE_OFFSET_BITS=64 -DGIT_COMMIT=<commit> -T data_sections.ld -ffunction-sections -fdata-sections -ffile-prefix-map=/out/work=/usr/src/type2-runtime -Wl,--gc-sections -Wl,-Map=/out/link.map -static -Wall -Werror -static-pie; strip --strip-debug --strip-unneeded; patch AI\\x02 at offset 8"},
                "upstream_patch": {"path": "patches/libfuse/mount.c.diff", **lock["sources"]["libfuse_patch"]},
                "linked_archives": archive_map, "linked_objects": object_map,
                "components": components,
                "linked_source_closure": {
                    "lock_sha256": records["relink/alpine-source.lock.json"],
                    "local_cache_path": str(cache / "alpine-sources"),
                    "verified_input_count": source_count,
                    "distribution_status": "local qualification evidence only; full cache not bundled",
                },
                "modified_libfuse": {
                    "enabled": modified_hash is not None,
                    "original_sha256": lock["sources"]["libfuse"]["sha256"],
                    "modified_sha256": modified_hash,
                },
                "notice_files": {name: digest for name, digest in records.items()
                                 if name.startswith("licenses/") and
                                 any(name in component["notice_files"] for component in components)},
                "relink_files": {name: digest for name, digest in records.items()
                                 if name.startswith("relink/")},
                "recipe_sha256": {file: sha256(HERE / file) for file in
                                  ("build.py", "container-build.sh", "relink.sh")},
                "files_sha256": records,
                "license_note": ("LGPL-2.1 libfuse source, patch and rebuild script supplied as qualification "
                                 "materials. FUR-005 must make complete matching source/toolchain inputs "
                                 "available alongside distributed AppImage and approve terms; local cache "
                                 "and URLs alone are not distribution compliance.") }
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"Built {artifact} SHA256 {records['runtime-x86_64']}; manifest {output / 'manifest.json'}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="new absolute output directory")
    parser.add_argument("--source-cache", type=Path, help="optional reusable verified input cache outside output")
    parser.add_argument("--modified-libfuse", type=Path,
                        help="relink only: absolute interface-compatible unpatched FUSE source tarball")
    args = parser.parse_args()
    try:
        default_cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "furami" / "appimage-runtime"
        build(args.output, (args.source_cache or default_cache).resolve(), args.modified_libfuse)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
