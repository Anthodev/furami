#!/usr/bin/env python3
"""Assemble pinned Furami AppDir and AppImage; final audit is separate."""
import argparse
import hashlib
import importlib.util
import json
import os
import re
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
STACK_LOCK = REPO / "packaging/media/stack.lock.json"
TOOL_URL = "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage"
TOOL_SHA256 = "ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
# The only non-prefix absolute loader path observed in 84 staged ELF objects.
# Both byte hashes came from the signed-snapshot libpulse0 .deb below.
PULSE_DIR = "/usr/lib/x86_64-linux-gnu/pulseaudio"
PULSE_VERSION = "1:16.1+dfsg1-2ubuntu10.1"
PULSE_DEB_SHA256 = "b8d52aa6c2f74ae99e749c9f98d6ad2dbf924fd422371ac469725245cdd919fc"
PULSE_ELF_SHA256 = {
    "libpulse.so.0": "3d78bd6bfcebccb7d4e65bac298c3aef5b0906fc4c6a4bebcd761c845f4c3c4a",
    "libpulsecommon-16.1.so": "00ab68a64ecbb5d6e0d4b5b35707a83a9040cdb373008e487176e49d184cf22f",
}
XCB_PLUGINS = (
    "plugins/platforms/libqxcb.so",
    "plugins/xcbglintegrations/libqxcb-egl-integration.so",
    "plugins/xcbglintegrations/libqxcb-glx-integration.so",
    "plugins/imageformats/libqsvg.so",
    "plugins/iconengines/libqsvgicon.so",
)
# Exact host ABI contract. X11/XCB/xkbcommon belong to pinned application closure.
HOST_LIBS = frozenset({
    "ld-linux-x86-64.so.2", "libc.so.6", "libm.so.6", "libdl.so.2",
    "libpthread.so.0", "librt.so.1", "libresolv.so.2", "libutil.so.1",
    "libanl.so.1", "libGL.so.1", "libEGL.so.1", "libGLX.so.0",
    "libGLdispatch.so.0", "libOpenGL.so.0", "libvulkan.so.1",
    "libdrm.so.2", "libgbm.so.1",
})
NEEDED = re.compile(r"\(NEEDED\).*?\[([^]]+)\]")
PATH_TAG = re.compile(r"\((RUNPATH|RPATH)\).*?\[([^]]*)\]")
SONAME = re.compile(r"\(SONAME\).*?\[([^]]*)\]")
# Upstream KDAB v0.10.0 tag commit 61562797ba25558a7c7dba8017deca038f3ee9e4.
# These are upstream LICENSES/* bytes, not fabricated LICENSE files in .crate archives.
KDAB_LICENSE_SHA256 = {
    "MIT.txt": "b85dcd3e453d05982552c52b5fc9e0bdd6d23c6f8e844b984a88af32570b0cc0",
    "Apache-2.0.txt": "074e6e32c86a4c0ef8b3ed25b721ca23aca83df277cd88106ef7177c354615ff",
}
KDAB_TAG = "61562797ba25558a7c7dba8017deca038f3ee9e4"


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def snapshot(root):
    """Record files, symlink text and permission bits; symlink targets never followed."""
    result = {}
    for parent, directories, files in os.walk(root, followlinks=False):
        for name in sorted(directories + files):
            item = Path(parent) / name
            rel = item.relative_to(root).as_posix()
            if item.is_symlink():
                target = os.readlink(item)
                result[rel] = {"type": "symlink", "target": target,
                               "sha256": hashlib.sha256(os.fsencode(target)).hexdigest()}
                if os.path.isabs(target) or not item.exists() or not item.resolve().is_relative_to(root.resolve()):
                    raise ValueError(f"unsafe symlink: {item} -> {target}")
            elif item.is_file():
                result[rel] = {"type": "file", "sha256": digest(item),
                               "mode": oct(stat.S_IMODE(item.stat().st_mode))}
            elif item.is_dir():
                result[rel] = {"type": "directory", "mode": oct(stat.S_IMODE(item.stat().st_mode))}
            else:
                raise ValueError(f"unexpected staged entry: {item}")
    return result


def normalize_mtimes(root, epoch):
    # Squashfs stores per-entry timestamps, including directory/symlink metadata.
    # Do not change input artifacts, only our copied AppDir payload.
    for parent, directories, files in os.walk(root, topdown=False, followlinks=False):
        for name in directories + files:
            os.utime(Path(parent) / name, (epoch, epoch), follow_symlinks=False)
    os.utime(root, (epoch, epoch))


def verify_artifacts(root, artifacts):
    for rel, expected in artifacts.items():
        if not isinstance(rel, str) or not rel or rel.startswith("/") or ".." in Path(rel).parts:
            raise ValueError(f"invalid artifact path: {rel}")
        path = root / rel
        if expected.get("type") == "symlink":
            if not path.is_symlink():
                raise ValueError(f"artifact mismatch: {rel}")
            actual_target = os.readlink(path)
            if os.path.isabs(actual_target) or not path.exists() or not path.resolve().is_relative_to(root.resolve()):
                raise ValueError(f"unsafe symlink: {rel} -> {actual_target}")
            target = expected.get("target")
            if actual_target != target:
                raise ValueError(f"artifact mismatch: {rel}")
            actual = hashlib.sha256(os.fsencode(target)).hexdigest()
        elif expected.get("type") == "file":
            if path.is_symlink() or not path.is_file():
                raise ValueError(f"artifact mismatch: {rel}")
            actual = digest(path)
        else:
            raise ValueError(f"invalid artifact type: {rel}")
        if actual != expected.get("sha256"):
            raise ValueError(f"artifact mismatch: {rel}")


def compare_payload(staged, extracted):
    a, b = snapshot(staged), snapshot(extracted)
    if a != b:
        wrong = next((p for p in sorted(a.keys() | b.keys()) if a.get(p) != b.get(p)), "root")
        raise ValueError(f"payload mismatch: {wrong}: staged={a.get(wrong)}, extracted={b.get(wrong)}")
    return a


def runtime_md5_field(source):
    """Find pinned type-2 runtime's sole 16-byte appimagetool write via ELF64 SHDR."""
    if len(source) < 64 or source[:6] != b"\x7fELF\x02\x01":
        raise ValueError("runtime ELF section header missing/unsupported (ELF64 little-endian required)")
    table = struct.unpack_from("<Q", source, 40)[0]
    stride, count, names_index = struct.unpack_from("<HHH", source, 58)
    if stride < 64 or count < 3 or names_index == 0 or names_index >= count or table < 64 or table + stride * count > len(source):
        raise ValueError("runtime ELF section table invalid")

    def section(index):
        return struct.unpack_from("<IIQQQQIIQQ", source, table + index * stride)

    names_header = section(names_index)
    names_offset, names_length = names_header[4:6]
    if names_header[1] != 3 or names_offset + names_length > len(source):
        raise ValueError("runtime ELF section name table invalid")
    names = source[names_offset:names_offset + names_length]
    matches = []
    for index in range(count):
        header = section(index)
        start = header[0]
        end = names.find(b"\0", start)
        if start >= len(names) or end < 0:
            raise ValueError("runtime ELF section name invalid")
        if names[start:end] == b".digest_md5":
            offset, length = header[4:6]
            if header[1] != 1 or offset == 0 or length < 16 or offset + length > len(source):
                raise ValueError("runtime ELF .digest_md5 section invalid")
            matches.append((offset, length))
    if len(matches) != 1:
        raise ValueError("runtime ELF .digest_md5 section absent/ambiguous")
    return matches[0]


def verify_runtime_header(image, runtime):
    original = runtime.read_bytes()
    if image.stat().st_size <= len(original):
        raise ValueError("runtime header present without AppImage payload")
    with image.open("rb") as stream:
        written = stream.read(len(original))
    offset, section_size = runtime_md5_field(original)
    end = offset + 16
    if written[:offset] != original[:offset] or written[end:] != original[end:]:
        raise ValueError("runtime header changed outside appimagetool .digest_md5 16-byte field")
    before, after = original[offset:end], written[offset:end]
    if before == after:
        raise ValueError("runtime header .digest_md5 write not observed")
    return {"source_sha256": hashlib.sha256(original).hexdigest(),
            "image_header_sha256": hashlib.sha256(written).hexdigest(),
            "bytes": len(original), "comparison": "exact except first 16 bytes of ELF .digest_md5",
            "md5_field": {"section": ".digest_md5", "section_size": section_size,
                          "offset": offset, "length": 16, "before_hex": before.hex(),
                          "after_hex": after.hex(),
                          "upstream_write": "AppImage/appimagetool 1.9.1 src/appimagetool.c:1073-1127",
                          "digest_semantics": "unverified: pinned src/digest.c hashes uninitialized skipped bytes"}}


def run(argv, *, cwd=None, env=None):
    completed = subprocess.run([str(a) for a in argv], cwd=cwd, env=env, text=True,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if completed.returncode:
        raise ValueError(f"{argv[0]} exited {completed.returncode}: {completed.stderr.strip() or completed.stdout.strip()}")
    return completed.stdout


def read_manifest(root):
    path = root / "manifest.json"
    obj = json.loads(path.read_text())
    if obj.get("schema") != 1:
        raise ValueError(f"unsupported manifest: {path}")
    return obj, digest(path)


def verify_app_reproducibility(app_root, app):
    """Verify selected single-build inputs; reproducibility belongs to CI."""
    builds = app.get("builds", {})
    selected = app_root / "bin/furami"
    first = app_root / "first/bin/furami"
    if builds.get("status") != "single-build":
        raise ValueError("expected single-build app receipt")
    sha = digest(selected)
    if selected.is_symlink() or first.is_symlink() or digest(first) != sha or builds.get("sha256_first") != sha:
        raise ValueError("application selected ELF differs from independent build receipt")
    inputs = app.get("inputs", {})
    if not inputs or "packaging/appimage/build-app.py" not in inputs:
        raise ValueError("application source snapshot missing")
    for relative, expected in inputs.items():
        if Path(relative).is_absolute() or ".." in Path(relative).parts:
            raise ValueError(f"unsafe application source path: {relative}")
        for root in (REPO, app_root / "source-snapshot"):
            path = root / relative
            if path.is_symlink() or not path.is_file() or digest(path) != expected:
                raise ValueError(f"application source snapshot changed: {relative}")
    return {"status": "single-build", "sha256_first": sha,
            "sha256_second": None, "byte_identical": None}


def check_inputs(media_root, app_root, runtime_root):
    media, media_hash = read_manifest(media_root)
    app, app_hash = read_manifest(app_root)
    runtime, runtime_hash = read_manifest(runtime_root)
    lock_hash = digest(STACK_LOCK)
    if media.get("lock_sha256") != lock_hash or app.get("media", {}).get("lock_sha256") != lock_hash or app["media"].get("manifest_sha256") != media_hash:
        raise ValueError("application not built against exact media manifest/stack lock")
    if app.get("container") != media.get("container") or app.get("apt_snapshot") != media.get("apt_snapshot"):
        raise ValueError("application Ubuntu container/APT provenance differs from media")
    app_build = verify_app_reproducibility(app_root, app)
    lock = json.loads(STACK_LOCK.read_text())
    if digest(REPO / "packaging/media/build-apt-closure.json") != lock["apt_closure"]["sha256"]:
        raise ValueError("pinned Ubuntu APT package closure changed")
    if not all((media_root / name).is_file() for name in ("apt-direct-packages.txt", "apt-debs.sha256", "apt-installed.tsv")):
        raise ValueError("matching media APT closure logs required")
    if digest(media_root / "apt-installed.tsv") != lock["apt_closure"]["installed_sha256"] or digest(media_root / "apt-debs.sha256") != lock["apt_closure"]["downloaded_debs_sha256"]:
        raise ValueError("media APT closure logs differ from lock")
    verify_artifacts(media_root / "prefix", media["artifacts"])
    binary = app.get("binary", {})
    if binary.get("path") != "bin/furami" or binary.get("sha256") != digest(app_root / "bin/furami"):
        raise ValueError("application executable checksum differs from build manifest")
    verify_artifacts(app_root, app["artifacts"])
    artifact = runtime.get("artifact", {})
    if artifact.get("path") != "runtime-x86_64" or artifact.get("sha256") != digest(runtime_root / "runtime-x86_64") or artifact.get("elf_machine") != "EM_X86_64" or not artifact.get("static"):
        raise ValueError("source-built runtime manifest/integrity/architecture invalid")
    if (app_root / "bin/furami").read_bytes()[:4] != b"\x7fELF" or (runtime_root / "runtime-x86_64").read_bytes()[:4] != b"\x7fELF":
        raise ValueError("expected ELF application and source-built runtime")
    return {"media": media_hash, "app": app_hash, "runtime": runtime_hash,
            "stack_lock": lock_hash, "app_build": app_build}, media


def relocated_runpath(value, relative):
    dest = Path(relative).parent
    tokens = []
    for token in value.split(":"):
        if token in ("/out/prefix/lib", "/media/prefix/lib"):
            path = os.path.relpath("usr/lib", dest)
            candidate = "$ORIGIN" + ("/" + path if path != "." else "")
            tokens.append(candidate if len(candidate) <= len(token) else "$ORIGIN")
        elif token == PULSE_DIR and relative == "usr/lib/libpulse.so.0":
            tokens.append("$ORIGIN/pulseaudio")
        elif token.startswith("/"):
            raise ValueError(f"foreign absolute RPATH/RUNPATH: {relative}: {token}")
        elif token.startswith(("$ORIGIN", "${ORIGIN}")):
            tokens.append(token)
        else:
            raise ValueError(f"unqualified RPATH/RUNPATH: {relative}: {token}")
    return ":".join(tokens)


def elf_metadata(path):
    if not path.is_file():
        return None
    with path.open("rb") as stream:
        if stream.read(4) != b"\x7fELF":
            return None
    header = run(["readelf", "-h", "-d", "--", path])
    if "Advanced Micro Devices X86-64" not in header or "ELF64" not in header:
        raise ValueError(f"non x86_64 ELF: {path}")
    paths = PATH_TAG.findall(header)
    return {"needed": NEEDED.findall(header), "paths": paths, "soname": SONAME.findall(header)}


def copy_component(source_root, src_rel, appdir, dest_rel, owners, owner):
    """Copy a verified file/symlink, preserving relative symlink chain and bytes."""
    current = source_root / src_rel
    dest = appdir / dest_rel
    if dest.is_symlink() or dest.exists():
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    if current.is_symlink():
        target = os.readlink(current)
        if os.path.isabs(target) or not current.resolve().is_relative_to(source_root.resolve()):
            raise ValueError(f"unsafe symlink: {current} -> {target}")
        dest.symlink_to(target)
        owners[dest_rel] = {"owner": owner, "source": src_rel, "source_sha256": hashlib.sha256(os.fsencode(target)).hexdigest()}
        next_source = (Path(src_rel).parent / target).as_posix()
        next_dest = (Path(dest_rel).parent / target).as_posix()
        copy_component(source_root, next_source, appdir, next_dest, owners, owner)
    else:
        if not current.is_file():
            raise ValueError(f"missing component: {current}")
        shutil.copy2(current, dest)
        owners[dest_rel] = {"owner": owner, "source": src_rel, "source_sha256": digest(current)}


def copy_prefix(prefix, manifest, src_rel, appdir, dest_rel, owners):
    if src_rel not in manifest:
        raise ValueError(f"prefix artifact unrecorded: {src_rel}")
    if dest_rel in owners and owners[dest_rel]["source"] != src_rel:
        raise ValueError(f"destination ownership conflict: {dest_rel}")
    copy_component(prefix, src_rel, appdir, dest_rel, owners, "media")
    # Validate entire symlink chain against source manifest, not only first entry.
    item = prefix / src_rel
    while item.is_symlink():
        src_rel = (Path(src_rel).parent / os.readlink(item)).as_posix()
        if src_rel not in manifest:
            raise ValueError(f"symlink target unrecorded: {src_rel}")
        item = prefix / src_rel


def stage_xcb_plugins(prefix, manifest, appdir, owners):
    # xcb alone can create the QML root but not Qt Quick's GL context.
    for relative in XCB_PLUGINS:
        entry = manifest.get(relative)
        original = prefix / relative
        if (not isinstance(entry, dict) or entry.get("type") != "file"
                or not original.is_file() or original.is_symlink()
                or digest(original) != entry.get("sha256")):
            raise ValueError(f"required pinned Qt XCB graphics plugin missing/mismatched: {relative}")
        category = relative.split("/", 1)[1]
        copy_prefix(prefix, manifest, relative, appdir, "usr/lib/qt6/plugins/" + category, owners)


def scan_qml(prefix, appdir, manifest, owners):
    scanner = prefix / "libexec/qmlimportscanner"
    if not scanner.is_file():
        scanner = prefix / "bin/qmlimportscanner"
    if not scanner.is_file():
        raise ValueError("matching Qt qmlimportscanner absent from media prefix")
    # Explicit Basic selection: QML Controls style lookup otherwise depends on
    # system/user configuration and escapes scanner's static module closure.
    with tempfile.TemporaryDirectory() as tmp:
        qml = Path(tmp)
        (qml / "style.qml").write_text("import QtQuick\nimport QtQuick.Controls.Basic\nItem {}\n")
        env = {**os.environ, "LD_LIBRARY_PATH": str(prefix / "lib")}
        data = json.loads(run([scanner, "-rootPath", REPO / "qml", "-importPath", prefix / "qml"], env=env))
        data += json.loads(run([scanner, "-rootPath", qml, "-importPath", prefix / "qml"], env=env))
    modules = {}
    for entry in data:
        if entry.get("type") != "module":
            continue
        location = entry.get("path")
        if entry.get("name") == "dev.antho.furami" and not location:
            continue  # CXX-Qt module compiled into app, not a deployable Qt directory.
        if not location or not Path(location).is_relative_to(prefix / "qml") or not Path(location).is_dir():
            raise ValueError(f"QML import unresolved outside pinned Qt: {entry}")
        module = Path(location).relative_to(prefix / "qml").as_posix()
        modules[module] = {"name": entry.get("name"), "version": entry.get("version")}
    for required in ("QtQuick", "QtQuick/Controls", "QtQuick/Controls/Basic",
                     "QtQuick/Controls/impl", "QtQuick/Templates"):
        if required not in modules:
            # Scanner may not list implicit control dependencies: seed only
            # known runtime style imports; never claim scanner observed them.
            if not (prefix / "qml" / required / "qmldir").is_file():
                raise ValueError(f"required Qt QML module missing: {required}")
            modules[required] = {"name": required.replace("/", "."), "implicit_style": True}
    for module in sorted(modules):
        base = prefix / "qml" / module
        for parent, dirs, files in os.walk(base, followlinks=False):
            current = Path(parent)
            # Adjacent nested qmldir entries are separate QML modules, never
            # silently stage unscanned Controls styles or unrelated modules.
            dirs[:] = sorted(d for d in dirs if not (current / d / "qmldir").exists())
            for name in sorted(files):
                entry = current / name
                rel = entry.relative_to(prefix).as_posix()
                copy_prefix(prefix, manifest, rel, appdir, "usr/" + rel, owners)
    return {"scanner": scanner.relative_to(prefix).as_posix(), "imports": data,
            "modules": modules, "style": "Basic"}


def host_library(soname):
    if "/" in soname or soname.startswith("."):
        raise ValueError(f"unsafe DT_NEEDED: {soname}")
    for directory in ("/lib/x86_64-linux-gnu", "/usr/lib/x86_64-linux-gnu", "/lib64"):
        candidate = Path(directory) / soname
        if candidate.is_file():
            return candidate
    raise ValueError(f"unresolved DT_NEEDED in Ubuntu image: {soname}")


def pulse_common_path(needed, requester, owner, original_paths):
    if (needed == "libpulsecommon-16.1.so" and requester == "usr/lib/libpulse.so.0"
            and owner == f"ubuntu:libpulse0={PULSE_VERSION}"
            and original_paths == [("RUNPATH", PULSE_DIR)]):
        return "usr/lib/pulseaudio/" + needed, Path(PULSE_DIR) / needed
    return None


def ubuntu_library_owner(package, version, architecture, binary, notice, installed, debs, base):
    if (package, version, architecture) not in installed:
        raise ValueError(f"Ubuntu library not in exact APT closure: {package}={version}:{architecture}")
    filename = f"{package}_{version.replace(':', '%3a')}_{architecture}.deb"
    if filename in debs:
        return {"origin": "signed-ubuntu-snapshot-deb", "package": package,
                "version": version, "architecture": architecture,
                "deb_filename": filename, "deb_sha256": debs[filename]}
    if (base.get("image") != json.loads(STACK_LOCK.read_text())["container"]
            or base.get("packages", {}).get(package) != {"version": version, "architecture": architecture}):
        raise ValueError(f"Ubuntu library package absent from base OCI: {package}={version}:{architecture}")
    recorded = base.get("files_sha256", {})
    binary_hash, notice_hash = digest(binary), digest(notice)
    if recorded.get(str(binary.resolve())) != binary_hash or recorded.get(str(notice.resolve())) != notice_hash:
        raise ValueError(f"Ubuntu base library/notice differs from immutable OCI: {binary}, {notice}")
    return {"origin": "pinned-ubuntu-base-image", "package": package,
            "version": version, "architecture": architecture,
            "container": base["image"],
            "base_library_sha256": binary_hash, "base_notice_sha256": notice_hash}


def base_image_provenance(image):
    command = ["podman", "run", "--rm", "--pull=always", "--platform", "linux/amd64",
               "--security-opt", "label=disable", "--volume", f"{REPO}:/workspace:ro",
               image, "sh", "/workspace/packaging/appimage/base-image-inventory.sh"]
    packages, files = {}, {}
    for line in run(command).splitlines():
        fields = line.split("\t")
        if len(fields) == 4 and fields[0] == "PACKAGE":
            _, package, version, architecture = fields
            if package in packages:
                raise ValueError(f"duplicate Ubuntu base package: {package}")
            packages[package] = {"version": version, "architecture": architecture}
        elif fields[0] == "FILE":
            match = re.fullmatch(r"([0-9a-f]{64})  (/.+)", line[5:])
            if not match or match.group(2) in files:
                raise ValueError(f"invalid Ubuntu base file identity: {line}")
            files[match.group(2)] = match.group(1)
        else:
            raise ValueError(f"unrecognized Ubuntu base provenance row: {line}")
    if not packages or not files:
        raise ValueError("Ubuntu base OCI package/file inventory empty")
    closure = json.loads((REPO / "packaging/media/build-apt-closure.json").read_text())
    downloaded = {row["filename"] for row in closure["downloaded_debs"]}
    missing, base_only = [], []
    for row in closure["installed"]:
        filename = f"{row['package']}_{row['version'].replace(':', '%3a')}_{row['architecture']}.deb"
        if filename in downloaded:
            continue
        if packages.get(row["package"]) == {"version": row["version"], "architecture": row["architecture"]}:
            base_only.append(row)
        else:
            missing.append(f"{row['package']}={row['version']}:{row['architecture']}")
    if missing:
        raise ValueError(f"APT installed packages with neither .deb nor exact OCI base row: {missing}")
    if len(downloaded) + len(base_only) != len(closure["installed"]):
        raise ValueError("APT closure contains unexpected downloaded/base package overlap")
    return {"schema": 1, "image": image, "packages": packages, "files_sha256": files,
            "probe_sha256": digest(HERE / "base-image-inventory.sh"),
            "installed_rows_checked": len(closure["installed"]),
            "downloaded_deb_rows": len(downloaded),
            "base_only_packages": sorted(base_only, key=lambda row: row["package"])}


def stage_libraries(prefix, manifest, appdir, owners, base_origin):
    prefix_lib = prefix / "lib"
    available = {p.name: p.relative_to(prefix).as_posix() for p in prefix_lib.iterdir()
                 if p.is_file() or p.is_symlink()}
    closure = json.loads((REPO / "packaging/media/build-apt-closure.json").read_text())
    debs = {row["filename"]: row["sha256"] for row in closure["downloaded_debs"]}
    packages = {(row["package"], row["version"], row["architecture"]) for row in closure["installed"]}
    host = {}
    elf = {}
    transformations = []
    while True:
        pending = []
        for rel, entry in sorted(owners.items()):
            path = appdir / rel
            if entry.get("type") != "elf-checked" and path.is_file() and not path.is_symlink():
                pending.append(rel)
        if not pending:
            break
        for rel in pending:
            owners[rel]["type"] = "elf-checked"
            target = appdir / rel
            metadata = elf_metadata(target)
            if metadata is None:
                continue
            if (any(PULSE_DIR in value.split(":") for _, value in metadata["paths"])
                    and owners[rel].get("owner") != f"ubuntu:libpulse0={PULSE_VERSION}"):
                raise ValueError(f"unqualified Ubuntu Pulse RUNPATH owner: {rel}")
            for kind, value in metadata["paths"]:
                replacement = relocated_runpath(value, rel)
                if replacement != value:
                    old = value.encode() + b"\0"
                    source = target.read_bytes()
                    # Dynamic string may also occur elsewhere; require exactly one
                    # NUL-terminated copy to avoid patching unrelated data.
                    if source.count(old) != 1 or len(replacement) > len(value):
                        raise ValueError(f"cannot safely relocate {kind} in {rel}: {value} -> {replacement}")
                    target.write_bytes(source.replace(old, replacement.encode() + b"\0" * (len(old) - len(replacement))))
                    transformations.append({"path": rel, "tag": kind, "before": value,
                                            "after": replacement, "before_sha256": owners[rel]["source_sha256"],
                                            "after_sha256": digest(target)})
            shipped = elf_metadata(target)
            if shipped["needed"] != metadata["needed"]:
                raise ValueError(f"ELF dependency names changed during RPATH relocation: {rel}")
            for _, search in shipped["paths"]:
                if any(segment.startswith("/") for segment in search.split(":")):
                    raise ValueError(f"absolute build RPATH survived relocation: {rel}: {search}")
            elf[rel] = {"before": metadata, "after": shipped}
            for needed in metadata["needed"]:
                nested = pulse_common_path(needed, rel, owners[rel]["owner"], metadata["paths"])
                dest = nested[0] if nested else "usr/lib/" + needed
                if (appdir / dest).exists():
                    continue
                if not nested and needed in available:
                    copy_prefix(prefix, manifest, available[needed], appdir, dest, owners)
                    continue
                if not nested and needed in HOST_LIBS:
                    host.setdefault(needed, {"path": str(host_library(needed)), "reason": "host core/graphics ABI"})
                    continue
                source = nested[1] if nested else host_library(needed)
                resolved = source.resolve()
                if not source.is_file():
                    raise ValueError(f"missing Ubuntu loader library: {source}")
                if "dri" in resolved.parts or re.search(r"(?:_dri|_icd|vulkan_[a-z0-9]+)\.so(?:\..*)?$", needed):
                    raise ValueError(f"GPU driver/ICD must remain host-owned: {needed} -> {resolved}")
                package = run(["dpkg-query", "-S", str(resolved)]).splitlines()[0].split(": ", 1)[0].split(":", 1)[0]
                version = run(["dpkg-query", "-W", "-f=${Version}", package]).strip()
                architecture = run(["dpkg-query", "-W", "-f=${Architecture}", package]).strip()
                copyright_file = Path("/usr/share/doc") / package / "copyright"
                if not copyright_file.is_file():
                    raise ValueError(f"Ubuntu library lacks redistributable notice: {package}: {needed}")
                attribution = ubuntu_library_owner(package, version, architecture, resolved,
                                                   copyright_file, packages, debs, base_origin)
                if package == "libpulse0":
                    if (version != PULSE_VERSION or needed not in PULSE_ELF_SHA256
                            or attribution.get("deb_sha256") != PULSE_DEB_SHA256
                            or digest(resolved) != PULSE_ELF_SHA256[needed]):
                        raise ValueError(f"Ubuntu Pulse library bytes differ from verified snapshot .deb: {needed}")
                if nested and package != "libpulse0":
                    raise ValueError(f"Pulse RUNPATH target has wrong package owner: {resolved}")
                copy_component(resolved.parent, resolved.name, appdir, dest, owners, f"ubuntu:{package}={version}")
                notice = appdir / "usr/share/doc/ubuntu" / package / "copyright"
                notice.parent.mkdir(parents=True, exist_ok=True)
                if not notice.exists():
                    shutil.copy2(copyright_file, notice)
                    owners[notice.relative_to(appdir).as_posix()] = {
                        "owner": f"ubuntu:{package}={version}", "source": str(copyright_file),
                        "source_sha256": digest(copyright_file)}
                owners[dest]["ubuntu_path"] = str(resolved)
                owners[dest].update(attribution)
    return {"elf": elf, "host_exclusions": host, "transformations": transformations}


def cached_download(cache, filename, url, expected):
    cache.mkdir(parents=True, exist_ok=True)
    target = cache / filename
    if not target.exists():
        with urllib.request.urlopen(url, timeout=180) as source, tempfile.NamedTemporaryFile(dir=cache, delete=False) as temp:
            temporary = Path(temp.name)
            shutil.copyfileobj(source, temp)
        try:
            if digest(temporary) != expected:
                raise ValueError(f"download checksum mismatch: {url}")
            temporary.replace(target)
        finally:
            temporary.unlink(missing_ok=True)
    if digest(target) != expected:
        raise ValueError(f"cached checksum mismatch: {target}")
    return target


def crate_notices(app_root, app_manifest, appdir, owners, kdab_licenses=None):
    records = []
    for crate in app_manifest.get("crates", []):
        vendor_rel = crate.get("vendor_path")
        if vendor_rel is None and crate.get("name") == "furami" and crate.get("source") is None:
            records.append({"name": "furami", "version": crate["version"], "role": crate["role"],
                            "source": "repository/LICENSE", "checksum": crate.get("checksum"),
                            "license_expression": crate.get("license_expression"),
                            "notices": ["usr/share/licenses/furami/LICENSE"]})
            continue
        if (not isinstance(vendor_rel, str) or vendor_rel !=
                f"first/vendor/{crate.get('name')}-{crate.get('version')}" or ".." in Path(vendor_rel).parts):
            raise ValueError(f"crate vendor provenance missing/unsafe: {crate.get('name')}")
        root = app_root / vendor_rel
        if not root.is_dir() or root.is_symlink():
            raise ValueError(f"crate source unavailable: {vendor_rel}")
        special = crate["name"] in ("cxx-qt", "cxx-qt-lib") and crate["version"] == "0.10.0"
        notices, provenance = [], {}
        if special and crate["role"] == "shipped-code-candidate":
            archive = app_root / "sources" / f"{crate['name']}-{crate['version']}.crate"
            if not archive.is_file() or archive.is_symlink():
                raise ValueError(f"crate source archive unavailable: {archive}")
            if digest(archive) != crate.get("checksum"):
                raise ValueError(f"crate archive checksum mismatch: {archive}")
            metadata = tomllib.loads((root / "Cargo.toml").read_text())["package"]
            if any(metadata.get(key) != crate.get(field) for key, field in
                   (("name", "name"), ("version", "version"), ("license", "license_expression"))):
                raise ValueError(f"crate Cargo metadata differs: {vendor_rel}")
            with tarfile.open(archive, "r:gz") as original:
                cargo = original.extractfile(f"{crate['name']}-{crate['version']}/Cargo.toml")
                if cargo is None or tomllib.loads(cargo.read().decode())["package"]["license"] != metadata["license"]:
                    raise ValueError(f"crate archive Cargo metadata differs: {vendor_rel}")
                checksums = json.loads((root / ".cargo-checksum.json").read_text())["files"]
                for relative, expected in checksums.items():
                    if not relative.startswith(("src/", "include/")) or Path(relative).suffix not in (".rs", ".cpp", ".h"):
                        continue
                    member = original.getmember(f"{crate['name']}-{crate['version']}/{relative}")
                    if not member.isfile() or hashlib.sha256(original.extractfile(member).read()).hexdigest() != expected:
                        raise ValueError(f"crate archive source hash mismatch: {vendor_rel}/{relative}")
            if metadata["license"] != "MIT OR Apache-2.0" or kdab_licenses is None:
                raise ValueError(f"linked KDAB license source unavailable: {vendor_rel}")
            checksums = json.loads((root / ".cargo-checksum.json").read_text())["files"]
            attribution = []
            for relative, expected in sorted(checksums.items()):
                if not relative.startswith(("src/", "include/")) or Path(relative).suffix not in (".rs", ".cpp", ".h"):
                    continue
                source = root / relative
                if source.is_symlink() or not source.is_file() or digest(source) != expected:
                    raise ValueError(f"crate source unavailable or mismatched: {vendor_rel}/{relative}")
                headers = [line for line in source.read_text().splitlines()[:20] if "SPDX-" in line]
                if (not any("SPDX-FileCopyrightText:" in line for line in headers)
                        or not any("SPDX-License-Identifier: MIT OR Apache-2.0" in line for line in headers)):
                    raise ValueError(f"linked KDAB attribution missing: {vendor_rel}/{relative}")
                attribution.extend((f"{relative} sha256:{expected}", *headers, ""))
                provenance[relative] = expected
            if not provenance:
                raise ValueError(f"linked KDAB attribution absent: {vendor_rel}")
            destination = f"usr/share/licenses/rust/{crate['name']}-{crate['version']}/SPDX-NOTICES.txt"
            target = appdir / destination
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("\n".join(attribution))
            owners[destination] = {"owner": "app-crate-source", "source": vendor_rel + "/src,include SPDX headers",
                                   "source_sha256": digest(target), "source_files_sha256": provenance,
                                   "crate_archive_sha256": crate["checksum"]}
            notices.append(destination)
            for filename, expected in KDAB_LICENSE_SHA256.items():
                source = kdab_licenses / filename
                if source.is_symlink() or not source.is_file() or digest(source) != expected:
                    raise ValueError(f"upstream KDAB license bytes unavailable/mismatched: {filename}")
                destination = f"usr/share/licenses/rust/{crate['name']}-{crate['version']}/{filename}"
                target = appdir / destination
                shutil.copy2(source, target)
                owners[destination] = {"owner": "KDAB-cxx-qt-v0.10.0",
                                       "source": f"https://github.com/KDAB/cxx-qt/blob/{KDAB_TAG}/LICENSES/{filename}",
                                       "source_sha256": expected}
                notices.append(destination)
        else:
            paths = {p for p in root.iterdir() if p.is_file() and p.name.upper().startswith(("LICENSE", "COPYING", "NOTICE"))}
            if crate.get("license_file"):
                declared = root / crate["license_file"]
                if not declared.is_file() or not declared.resolve().is_relative_to(root.resolve()):
                    raise ValueError(f"declared crate license missing/unsafe: {vendor_rel}: {crate['license_file']}")
                paths.add(declared)
            for source in sorted(paths):
                relative = source.relative_to(root).as_posix()
                destination = f"usr/share/licenses/rust/{crate['name']}-{crate['version']}/{relative}"
                target = appdir / destination
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, target)
                owners[destination] = {"owner": "app-crate-source", "source": f"{vendor_rel}/{relative}",
                                       "source_sha256": digest(source)}
                notices.append(destination)
        if crate["role"] == "shipped-code-candidate" and not notices:
            raise ValueError(f"linked crate attribution missing: {vendor_rel}")
        records.append({"name": crate["name"], "version": crate["version"], "role": crate["role"],
                        "source": crate.get("source"), "checksum": crate.get("checksum"),
                        "license_expression": crate.get("license_expression"), "notices": notices,
                        **({"source_files_sha256": provenance, "upstream_tag": KDAB_TAG} if special and provenance else {})})
    return records


def rust_std_notices(app_root, app_manifest, appdir, owners):
    std = app_manifest.get("binary", {}).get("rust_standard_library")
    toolchain = app_manifest.get("toolchain", {})
    if (not isinstance(std, dict) or std.get("version") != "1.97.1"
            or std.get("target") != "x86_64-unknown-linux-gnu"
            or std.get("license_expression") != "MIT OR Apache-2.0"):
        raise ValueError("linked Rust standard library manifest missing/invalid")
    source = std.get("source", {})
    if (source.get("archive") != toolchain.get("rust-src", {}).get("archive")
            or source.get("sha256") != toolchain.get("rust-src", {}).get("sha256")
            or source.get("path") != "rust-src-1.97.1/rust-src/lib/rustlib/src/rust/library"):
        raise ValueError("Rust standard library source provenance differs from pinned rust-src")
    src_archive = app_root / "sources" / source["archive"]
    if not src_archive.is_file() or digest(src_archive) != source["sha256"]:
        raise ValueError("Rust standard library source archive missing/mismatched")
    linked = set(app_manifest["binary"].get("linked_archives", []))
    archives = std.get("archives")
    if not isinstance(archives, list) or not archives:
        raise ValueError("linked Rust standard library archives absent")
    expected_archives = {path for path in linked if path.startswith(
        "first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/") and path.endswith(".rlib")}
    if {row.get("path") for row in archives} != expected_archives:
        raise ValueError("linked Rust standard library archive inventory incomplete")
    names = set()
    for archive in archives:
        relative = archive.get("path")
        if (not isinstance(relative, str) or relative not in linked
                or not relative.startswith("first/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/")
                or not Path(relative).name.startswith("lib") or not relative.endswith(".rlib")
                or archive.get("name") != Path(relative).name[3:-5].rsplit("-", 1)[0]
                or not isinstance(archive.get("source_path"), str)
                or archive["source_path"].startswith("/")
                or ".." in Path(archive["source_path"]).parts
                or not isinstance(archive.get("license_expression"), str)
                or not archive["license_expression"]
                or Path(relative).name in names):
            raise ValueError(f"Rust std linked archive provenance invalid: {relative}")
        names.add(Path(relative).name)
        file = app_root / relative
        if file.is_symlink() or not file.is_file() or digest(file) != archive.get("sha256"):
            raise ValueError(f"Rust std linked archive bytes mismatch: {relative}")
    component_notices = {}
    with tarfile.open(src_archive, "r:xz") as sources:
        for archive in archives:
            member = sources.getmember(source["path"] + "/" + archive["source_path"] + "/Cargo.toml")
            if not member.isfile():
                raise ValueError(f"Rust std crate source missing: {archive['source_path']}")
            metadata = tomllib.loads(sources.extractfile(member).read().decode())["package"]
            declared = metadata.get("license")
            if archive.get("declared_license", archive["license_expression"]) != declared:
                raise ValueError(f"Rust std crate license differs from source: {archive['source_path']}")
            if (declared != archive["license_expression"]
                    and not (archive["name"] == "rustc_demangle"
                             and declared == "MIT/Apache-2.0"
                             and archive["license_expression"] == "MIT OR Apache-2.0")):
                raise ValueError(f"Rust std crate license normalization unrecognized: {archive['source_path']}")
            if archive["source_path"].startswith("vendor/"):
                directory = source["path"] + "/" + archive["source_path"] + "/"
                found = []
                for notice in sources.getmembers():
                    if (not notice.isfile() or not notice.name.startswith(directory)
                            or "/" in notice.name[len(directory):]
                            or not notice.name[len(directory):].upper().startswith(("LICENSE", "COPYING", "NOTICE"))):
                        continue
                    content = sources.extractfile(notice).read()
                    destination = ("usr/share/licenses/rust/rust-std-1.97.1/vendor/"
                                   + archive["name"] + "/" + notice.name[len(directory):])
                    target = appdir / destination
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(content)
                    owners[destination] = {"owner": "rust-std-vendored-" + archive["name"],
                                           "source": f"{source['archive']}:{notice.name}",
                                           "source_archive_sha256": source["sha256"],
                                           "source_sha256": hashlib.sha256(content).hexdigest()}
                    found.append(destination)
                if not found:
                    raise ValueError(f"Rust std vendored crate notices absent: {archive['source_path']}")
                component_notices[archive["name"]] = sorted(found)
    if not any(name.startswith("libstd-") for name in names):
        raise ValueError("linked Rust standard library libstd absent")
    expected = {"LICENSE-APACHE", "LICENSE-MIT", "COPYRIGHT"}
    notices = std.get("notices")
    notice_archive = toolchain.get("rust-std", {})
    if (not isinstance(notices, list) or {Path(item.get("path", "")).name for item in notices} != expected
            or any(item.get("archive") != notice_archive.get("archive")
                   or item.get("path") != f"{notice_archive['archive'][:-7]}/{Path(item.get('path', '')).name}"
                   for item in notices)):
        raise ValueError("Rust std notice manifest missing/invalid")
    archive_path = app_root / "sources" / notice_archive["archive"]
    if not archive_path.is_file() or digest(archive_path) != notice_archive.get("sha256"):
        raise ValueError("Rust std distribution archive missing/mismatched")
    results = []
    with tarfile.open(archive_path, "r:xz") as bundle:
        for item in sorted(notices, key=lambda row: row["path"]):
            member = bundle.getmember(item["path"])
            if not member.isfile():
                raise ValueError(f"Rust std notice member not a regular file: {item['path']}")
            content = bundle.extractfile(member).read()
            if hashlib.sha256(content).hexdigest() != item.get("sha256"):
                raise ValueError(f"Rust std notice hash mismatch: {item['path']}")
            destination = "usr/share/licenses/rust/rust-std-1.97.1/" + Path(item["path"]).name
            target = appdir / destination
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
            owners[destination] = {"owner": "rust-std-1.97.1", "source": f"{notice_archive['archive']}:{item['path']}",
                                   "source_archive_sha256": notice_archive["sha256"], "source_sha256": item["sha256"]}
            results.append(destination)
    return {"version": std["version"], "target": std["target"], "license_expression": std["license_expression"],
            "source": source, "linked_archives": archives, "notices": results,
            "vendored_component_notices": component_notices}

def copy_runtime_materials(runtime_root, manifest, appdir, owners, lock):
    notices = manifest.get("notice_files")
    relink = manifest.get("relink_files")
    recorded = manifest.get("files_sha256")
    components = manifest.get("components")
    if any(not isinstance(value, dict) or not value for value in (notices, relink, recorded)) or not isinstance(components, list) or not components:
        raise ValueError("runtime notice/relink files or static components missing from manifest")
    required = {"type2-runtime.runtime.c.NOTICE", "gcc.COPYING3",
                "gcc.COPYING.RUNTIME", "libfuse.LGPL2.txt"}
    if not required.issubset({Path(name).name for name in notices}):
        raise ValueError(f"runtime notice set incomplete: {sorted(required - {Path(n).name for n in notices})}")
    for component in components:
        names = component.get("notice_files")
        if not isinstance(names, list) or not names or any(name not in notices for name in names):
            raise ValueError(f"runtime component notices unverified: {component.get('name')}")
    if not any(Path(path).name == "RELINKING.md" for path in relink) or not any(path.endswith(".sh") for path in relink):
        raise ValueError("runtime source-relink procedure/script missing")
    if manifest.get("sources") != lock["sources"]:
        raise ValueError("runtime sources differ from pinned source lock")
    for key in ("type2_runtime", "libfuse", "squashfuse", "libfuse_patch"):
        item = lock["sources"][key]
        matched = [hash_value for path, hash_value in relink.items() if Path(path).name == item["archive"]]
        if matched != [item["sha256"]]:
            raise ValueError(f"runtime source/patch archive unverified or absent: {item['archive']}")
    linked = manifest.get("linked_source_closure")
    if (not isinstance(linked, dict)
            or linked.get("lock_sha256") != relink.get("relink/alpine-source.lock.json")
            or linked.get("verified_input_count") != 60
            or not isinstance(linked.get("local_cache_path"), str)
            or not Path(linked["local_cache_path"]).is_absolute()
            or linked.get("distribution_status") != "local qualification evidence only; full cache not bundled"):
        raise ValueError("runtime linked source closure/local cache provenance incomplete")
    for category, entries in (("licenses", notices), ("relink", relink)):
        for relative, checksum in sorted(entries.items()):
            if not isinstance(relative, str) or not relative.startswith(category + "/") or ".." in Path(relative).parts:
                raise ValueError(f"unsafe runtime {category} path: {relative}")
            original = runtime_root / relative
            if (original.is_symlink() or not original.is_file()
                    or not original.resolve().is_relative_to(runtime_root.resolve())
                    or recorded.get(relative) != checksum or digest(original) != checksum):
                raise ValueError(f"runtime {category} bytes/manifest mismatch: {relative}")
            destination = "usr/share/furami/runtime-compliance/" + relative
            target = appdir / destination
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(original, target)
            owners[destination] = {"owner": "source-built-runtime", "source": relative,
                                   "source_sha256": checksum}
    return {"notice_files": notices, "relink_files": relink,
            "linked_source_closure": linked,
            "destination": "usr/share/furami/runtime-compliance"}


def internal(media_root, app_root, runtime_root, out, version):
    ids, media = check_inputs(media_root, app_root, runtime_root)
    if {p.name for p in out.iterdir()} != {"ubuntu-apt.log", "base-image-provenance.json",
                                             "stage-tool-closure.json"}:
        raise ValueError(f"qualification output must contain only Ubuntu APT/base/tool provenance logs: {out}")
    base_origin = json.loads((out / "base-image-provenance.json").read_text())
    closure = json.loads((REPO / "packaging/media/build-apt-closure.json").read_text())
    if (base_origin.get("image") != media["container"]
            or base_origin.get("probe_sha256") != digest(HERE / "base-image-inventory.sh")
            or base_origin.get("installed_rows_checked") != len(closure["installed"])
            or base_origin.get("downloaded_deb_rows") != len(closure["downloaded_debs"])):
        raise ValueError("Ubuntu base OCI provenance differs from pinned media closure")
    tool_lock = json.loads((HERE / "tool-closure.lock.json").read_text())
    tool_receipt = json.loads((out / "stage-tool-closure.json").read_text())
    if (tool_receipt.get("schema") != 1
            or tool_receipt.get("scope") != tool_lock.get("scope")
            or tool_receipt.get("lock_sha256") != digest(HERE / "tool-closure.lock.json")
            or tool_receipt.get("media_apt_closure_sha256") != digest(REPO / "packaging/media/build-apt-closure.json")
            or tool_receipt.get("media_installed_sha256") != digest(media_root / "apt-installed.tsv")
            or set(tool_receipt.get("packages", {})) != set(tool_lock["packages"])):
        raise ValueError("packaging-only tool closure receipt differs from frozen Ubuntu/media identity")
    for name, expected in tool_lock["packages"].items():
        recorded = tool_receipt["packages"][name]
        if any(recorded.get(key) != expected[key] for key in
               ("version", "architecture", "deb_filename", "deb_sha256")):
            raise ValueError(f"packaging-only package version/archive mismatch: {name}")
        if recorded.get("files_sha256") != expected["files_sha256"]:
            raise ValueError(f"packaging-only package file hashes mismatch: {name}")
        for relative, sha in expected["files_sha256"].items():
            if digest(Path(relative)) != sha:
                raise ValueError(f"packaging-only installed tool bytes differ: {relative}")
        if (recorded.get("copyright_sha256") != expected["copyright_sha256"]
                or digest(Path("/usr/share/doc") / name / "copyright") != expected["copyright_sha256"]):
            raise ValueError(f"packaging-only package copyright mismatch: {name}")
    appdir = out / "AppDir"
    appdir.mkdir()
    owners = {}
    prefix = media_root / "prefix"
    copy_component(app_root, "bin/furami", appdir, "usr/bin/furami", owners, "app")
    for pattern in ("libmpv.so*", "libplacebo.so*", "libav*.so*", "libsw*.so*"):
        matches = sorted((prefix / "lib").glob(pattern))
        if not matches:
            raise ValueError(f"missing mandatory shared component {pattern}")
        for match in matches:
            rel = match.relative_to(prefix).as_posix()
            copy_prefix(prefix, media["artifacts"], rel, appdir, "usr/" + rel, owners)
    stage_xcb_plugins(prefix, media["artifacts"], appdir, owners)
    qml = scan_qml(prefix, appdir, media["artifacts"], owners)
    for name in ("mpv", "ffmpeg", "libplacebo"):
        source = media["sources"][name]
        archive = Path("/media-sources") / source["archive"]
        if archive.is_symlink() or digest(archive) != source["sha256"]:
            raise ValueError(f"core source notice archive unverified: {name}")
        count = 0
        with tarfile.open(archive) as tar:
            for member in tar:
                parts = Path(member.name).parts
                if (len(parts) == 2 and member.isfile() and
                        parts[-1].startswith(("COPYING", "LICENSE", "Copyright"))):
                    relative = f"usr/share/licenses/media/{name}/{parts[-1]}"
                    target = appdir / relative
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(tar.extractfile(member).read())
                    owners[relative] = {"owner": "checked-source-notice",
                                        "source": source["archive"] + ":" + member.name,
                                        "source_sha256": digest(target)}
                    count += 1
        if not count:
            raise ValueError(f"core source archive lacks upstream notices: {name}")
    for relative in sorted(media["artifacts"]):
        if relative.startswith("sbom/") or relative.startswith("config_"):
            if (prefix / relative).is_file() and not (prefix / relative).is_symlink():
                copy_prefix(prefix, media["artifacts"], relative, appdir,
                            "usr/share/licenses/qt/" + relative, owners)
    icu = json.loads((HERE / "sources.lock.json").read_text())["icu_notice"]
    source = Path("/icu-LICENSE")
    if source.is_symlink() or digest(source) != icu["sha256"]:
        raise ValueError("checked ICU 73.2 notice bytes differ")
    target = appdir / "usr/share/licenses/icu/LICENSE"
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, target)
    owners[target.relative_to(appdir).as_posix()] = {
        "owner": "checked-ICU-notice", "source": icu["url"], "source_sha256": icu["sha256"]}
    for src, dest in ((HERE / "AppRun", "AppRun"), (HERE / "furami.desktop", "usr/share/applications/furami.desktop"),
                      (HERE / "furami.svg", "usr/share/icons/hicolor/scalable/apps/furami.svg"),
                      (HERE / "furami.png", "usr/share/icons/hicolor/256x256/apps/furami.png"),
                      (HERE / "qt.conf", "usr/bin/qt.conf"), (REPO / "LICENSE", "usr/share/licenses/furami/LICENSE"),
                      (HERE / "RELINKING.md", "usr/share/licenses/furami/RELINKING.md"),
                      (REPO / "packaging/media/LICENSE-AUDIT.md", "usr/share/licenses/furami/STACK-AUDIT.md")):
        target = appdir / dest
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, target)
        if dest == "AppRun":
            target.chmod(0o755)
        owners[dest] = {"owner": "qualification-template", "source": str(src.relative_to(REPO)), "source_sha256": digest(src)}
    (appdir / "furami.desktop").symlink_to("usr/share/applications/furami.desktop")
    (appdir / "furami.svg").symlink_to("usr/share/icons/hicolor/scalable/apps/furami.svg")
    (appdir / "furami.png").symlink_to("usr/share/icons/hicolor/256x256/apps/furami.png")
    (appdir / ".DirIcon").symlink_to("furami.png")
    dependencies = stage_libraries(prefix, media["artifacts"], appdir, owners, base_origin)
    app_manifest, _ = read_manifest(app_root)
    rust_crates = crate_notices(app_root, app_manifest, appdir, owners, Path("/kdab-licenses"))
    rust_standard_library = rust_std_notices(app_root, app_manifest, appdir, owners)
    for license_name in ("GPL-2", "GPL-3", "LGPL-2.1", "LGPL-3", "Apache-2.0"):
        source = Path("/usr/share/common-licenses") / license_name
        if not source.is_file():
            raise ValueError(f"Ubuntu license text unavailable: {license_name}")
        destination = f"usr/share/licenses/common/{license_name}"
        target = appdir / destination
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
        owners[destination] = {"owner": "ubuntu-license-text", "source": str(source),
                               "source_sha256": digest(source)}
    runtime_manifest, _ = read_manifest(runtime_root)
    if runtime_manifest.get("lock_sha256") != digest(REPO / "packaging/appimage-runtime/runtime.lock.json"):
        raise ValueError("runtime does not match pinned source/runtime lock")
    runtime_file = runtime_root / "runtime-x86_64"
    if elf_metadata(runtime_file)["needed"] or "INTERP" in run(["readelf", "-l", "--", runtime_file]):
        raise ValueError("runtime must be static x86_64 ELF without interpreter")
    runtime_materials = copy_runtime_materials(
        runtime_root, runtime_manifest, appdir, owners,
        json.loads((REPO / "packaging/appimage-runtime/runtime.lock.json").read_text()))
    source_offer = {"schema": 1, "scope": "build provenance; release-ready.json controls publication",
                    "media_sources": media["sources"], "media_lock_sha256": ids["stack_lock"],
                    "rust_crates": rust_crates, "rust_standard_library": rust_standard_library,
                    "app_toolchain": app_manifest.get("toolchain"),
                    "app_cargo_lock_sha256": app_manifest.get("cargo_lock_sha256"),
                    "app_source_files": app_manifest.get("inputs"),
                    "app_build_reproducibility": ids["app_build"],
                    "runtime_sources": runtime_manifest.get("sources"),
                    "runtime_apk_closure": runtime_manifest.get("apk_closure"),
                    "runtime_components": runtime_manifest.get("components"),
                    "runtime_linked_archives": runtime_manifest.get("linked_archives"),
                    "runtime_materials": {**runtime_materials,
                                          "linked_source_closure": {key: value for key, value in
                                                                    runtime_materials["linked_source_closure"].items()
                                                                    if key != "local_cache_path"}},
                    "runtime_build_flags": runtime_manifest.get("build_flags"),
                    "runtime_lgpl_warning": runtime_manifest.get("license_note"),
                    "qt_and_media_shared_libraries": "replaceable separate ELF .so files in usr/lib; isolated AppRun loader path",
                    "ubuntu_notices": "usr/share/doc/ubuntu/<binary-package>/copyright for every copied Ubuntu ELF"}
    offer_path = appdir / "usr/share/licenses/furami/source-offer.json"
    offer_path.write_text(json.dumps(source_offer, indent=2, sort_keys=True) + "\n")
    owners[offer_path.relative_to(appdir).as_posix()] = {"owner": "qualification-generated",
                                                          "source": "media/app/runtime manifests",
                                                          "source_sha256": digest(offer_path)}
    for rel, metadata in owners.items():
        metadata["shipped_sha256"] = digest(appdir / rel) if (appdir / rel).is_file() and not (appdir / rel).is_symlink() else metadata["source_sha256"]
    source_epoch = json.loads(STACK_LOCK.read_text())["source_date_epoch"]
    normalize_mtimes(appdir, source_epoch)
    payload = snapshot(appdir)
    with tempfile.TemporaryDirectory() as temp:
        temp = Path(temp)
        run(["/tool/appimagetool", "--appimage-extract"], cwd=temp)
        builder = temp / "squashfs-root/AppRun"
        if not builder.is_file():
            raise ValueError("pinned appimagetool extraction lacks AppRun")
        bundled_tools = {}
        for name, expected_sha in tool_lock["bundled_appimagetool_executables"].items():
            binary = temp / "squashfs-root/usr/bin" / name
            metadata = elf_metadata(binary)
            if (metadata is None or metadata["needed"] or digest(binary) != expected_sha
                    or not binary.stat().st_mode & stat.S_IXUSR):
                raise ValueError(f"pinned appimagetool bundled executable unverified: {name}")
            bundled_tools[name] = {"sha256": expected_sha, "dt_needed": [],
                                   "role": "build-only; not shipped"}
        image = out / f"Furami-{version}-x86_64.AppImage"
        env = {**os.environ, "ARCH": "x86_64", "SOURCE_DATE_EPOCH": str(json.loads(STACK_LOCK.read_text())["source_date_epoch"])}
        run([builder, "--no-appstream", "--runtime-file", runtime_root / "runtime-x86_64", appdir, image], env=env)
        image.chmod(0o755)
        runtime_header = verify_runtime_header(image, runtime_root / "runtime-x86_64")
        extract = temp / "payload"
        extract.mkdir()
        run([image, "--appimage-extract"], cwd=extract)
        extracted = extract / "squashfs-root"
        compare_payload(appdir, extracted)
    evidence = {"schema": 1, "qualification_only": False, "inputs_manifest_sha256": ids,
                "ubuntu_base_image": {"path": "base-image-provenance.json",
                                      "sha256": digest(out / "base-image-provenance.json"),
                                      "image": base_origin["image"],
                                      "apt_snapshot": media["apt_snapshot"],
                                      "installed_rows_checked": base_origin["installed_rows_checked"],
                                      "downloaded_deb_rows": base_origin["downloaded_deb_rows"]},
                "appimagetool": {"url": TOOL_URL, "sha256": TOOL_SHA256,
                                 "role": "build-only; not shipped",
                                 "bundled_executables": bundled_tools},
                "stage_only_ubuntu_tools": {"path": "stage-tool-closure.json",
                                            "sha256": digest(out / "stage-tool-closure.json"),
                                            "packages": tool_receipt["packages"],
                                            "file_version": tool_receipt["file_version"],
                                            "license_source": tool_lock["license_source"],
                                            "role": "build-only; not shipped"},
                "runtime_header": runtime_header, "runtime_materials": runtime_materials,
                "image": {"path": image.name, "sha256": digest(image),
                          "mode": oct(stat.S_IMODE(image.stat().st_mode)),
                          "app_build_status": ids["app_build"]["status"],
                          "app_binary_first_sha256": ids["app_build"]["sha256_first"],
                          "app_binary_second_sha256": ids["app_build"]["sha256_second"],
                          "app_binary_variance_documented": ids["app_build"]["status"] == "documented-variance",
                          "image_two_run_byte_identity": "not measured; never claimed",
                          "reproducibility_label": "single build; reproducibility measured only by CI build-twice.py"},
                "appdir": {"path": "AppDir", "source_date_epoch": source_epoch,
                           "artifacts": payload},
                "extraction": {"comparison": "byte/symlink/mode exact", "artifacts": len(payload)},
                "component_sources": owners, "qml": qml, "loader": dependencies,
                "runtime_loads": ["usr/lib/libmpv.so"]}
    (out / "evidence.json").write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    print(f"Qualified {image}; {len(payload)} payload files/symlinks")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--media-output", type=Path, required=True)
    parser.add_argument("--app-build", type=Path, required=True)
    parser.add_argument("--runtime-build", type=Path, required=True)
    parser.add_argument("--media-source-cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--tool-cache", type=Path, default=Path.home() / ".cache/furami-qualification")
    parser.add_argument("--in-container", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?", args.version):
        raise ValueError("unsafe image version")
    media, app, runtime, out = (p.resolve() for p in (args.media_output, args.app_build, args.runtime_build, args.output))
    if args.in_container:
        internal(media, app, runtime, out, args.version)
        return
    _, media_manifest = check_inputs(media, app, runtime)
    if out.exists() and any(out.iterdir()):
        raise ValueError(f"qualification output must be empty: {out}")
    lock = json.loads(STACK_LOCK.read_text())
    cache = args.tool_cache.resolve()
    tool = cached_download(cache, "appimagetool-1.9.1-x86_64.AppImage", TOOL_URL, TOOL_SHA256)
    tool.chmod(tool.stat().st_mode | stat.S_IXUSR)
    ca = lock["sources"]["ca_certificates"]
    trust = cached_download(cache, ca["archive"], ca["url"], ca["sha256"])
    kdab_dir = cache / "kdab-cxx-qt-v0.10.0"
    for filename, expected in KDAB_LICENSE_SHA256.items():
        cached_download(kdab_dir, filename,
                        f"https://raw.githubusercontent.com/KDAB/cxx-qt/{KDAB_TAG}/LICENSES/{filename}",
                        expected)
    icu = json.loads((HERE / "sources.lock.json").read_text())["icu_notice"]
    icu_notice = cached_download(cache, icu["archive"], icu["url"], icu["sha256"])
    base_origin = base_image_provenance(media_manifest["container"])
    out.mkdir(parents=True, exist_ok=True)
    (out / "base-image-provenance.json").write_text(json.dumps(base_origin, indent=2, sort_keys=True) + "\n")
    argv = ["podman", "run", "--rm", "--pull=always", "--platform", "linux/amd64",
            "--security-opt", "label=disable", "--volume", f"{REPO}:/workspace:ro",
            "--volume", f"{media}:/media:ro", "--volume", f"{app}:/app:ro",
            "--volume", f"{runtime}:/runtime:ro", "--volume", f"{out}:/out",
            "--volume", f"{args.media_source_cache.resolve()}:/media-sources:ro",
            "--volume", f"{tool}:/tool/appimagetool:ro",
            "--volume", f"{kdab_dir}:/kdab-licenses:ro",
            "--volume", f"{icu_notice}:/icu-LICENSE:ro",
            "--volume", f"{trust}:/bootstrap-ca.deb:ro", "--workdir", "/workspace",
            "--env", f"CA_SHA256={ca['sha256']}",
            "--env", f"APT_SNAPSHOT={lock['apt_snapshot']}",
            "--env", f"APT_INSTALLED_SHA256={lock['apt_closure']['installed_sha256']}",
            "--env", f"APT_DEBS_SHA256={lock['apt_closure']['downloaded_debs_sha256']}",
            "--env", f"FURAMI_VERSION={args.version}",
            media_manifest["container"], "bash", "packaging/appimage/setup-ubuntu.sh"]
    with (out / "ubuntu-apt.log").open("w") as log:
        status = subprocess.run(argv, stdout=log, stderr=subprocess.STDOUT, check=False).returncode
    if status:
        raise ValueError(f"Ubuntu staging exited {status}; see {out / 'ubuntu-apt.log'}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, json.JSONDecodeError) as error:
        sys.exit(f"qualification failed: {error}")
