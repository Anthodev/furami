#!/usr/bin/env python3
"""Download immutable sources, then build in a rootless Ubuntu container."""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import urllib.request
from pathlib import Path

LOCK_PATH = Path(__file__).with_name("stack.lock.json")
REQUIRED = {"ca_certificates", "mpv", "libplacebo", "vulkan_headers", "ffmpeg", "qt", "cxx_qt"}


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def check_lock(lock):
    if lock.get("schema") != 1 or set(lock.get("sources", {})) != REQUIRED:
        raise ValueError("expected schema 1 and seven frozen sources")
    if not re.fullmatch(r"docker.io/library/ubuntu@sha256:[0-9a-f]{64}", lock.get("container", "")):
        raise ValueError("Ubuntu container must use an immutable OCI digest")
    if lock.get("platform") != "linux/amd64" or not re.fullmatch(r"\d{8}T\d{6}Z", lock.get("apt_snapshot", "")):
        raise ValueError("Ubuntu platform and APT snapshot must be frozen")
    closure = lock.get("apt_closure", {})
    if closure.get("file") != "build-apt-closure.json" or any(
        not re.fullmatch(r"[0-9a-f]{64}", closure.get(key, ""))
        for key in ("sha256", "installed_sha256", "downloaded_debs_sha256")
    ):
        raise ValueError("APT closure must contain verified package versions and SHA256")
    if not isinstance(lock.get("source_date_epoch"), int):
        raise ValueError("SOURCE_DATE_EPOCH must be frozen")
    for name, source in lock["sources"].items():
        if name == "qt":
            continue
        if not re.fullmatch(r"[0-9a-f]{64}", source.get("sha256", "")):
            raise ValueError(f"{name}: missing or invalid SHA256")
        if not source.get("url", "").startswith("https://") or not re.fullmatch(r"[a-z0-9-]+\.(tar\.(gz|xz)|deb)", source.get("archive", "")):
            raise ValueError(f"{name}: immutable HTTPS source URL and filename required")
        if not source.get("version"):
            raise ValueError(f"{name}: version required")
    qt = lock["sources"]["qt"]
    required_qt = {"qtbase", "qtdeclarative", "qtsvg", "icu", "qtshadertools"}
    if qt.get("version") != "6.11.2" or qt.get("kit") != "linux_gcc_64":
        raise ValueError("Qt official 6.11.2 linux_gcc_64 kit required")
    archives = qt.get("archives", [])
    if len(archives) != len(required_qt) or {item.get("component") for item in archives} != required_qt:
        raise ValueError("Qt official kit archive closure incomplete")
    repository = "https://download.qt.io/online/qtsdkrepository/linux_x64/desktop/qt6_6112/qt6_6112/"
    for item in archives:
        if not re.fullmatch(r"[0-9a-f]{64}", item.get("sha256", "")):
            raise ValueError(f"Qt {item['component']} archive missing SHA256")
        if not re.fullmatch(r"[0-9a-f]{40}", item.get("upstream_sha1", "")):
            raise ValueError(f"Qt {item['component']} official SHA1 sidecar missing")
        filename = item.get("archive", "")
        package = ("qt.qt6.6112.addons.qtshadertools.linux_gcc_64"
                   if item["component"] == "qtshadertools" else "qt.qt6.6112.linux_gcc_64")
        expected = repository + package + "/" + filename
        if not re.fullmatch(r"6\.11\.2-0-[A-Za-z0-9_.-]+\.7z", filename) or item.get("url") != expected:
            raise ValueError(f"Qt {item['component']} archive must use official release URL")
    if not re.fullmatch(r"\d+\.\d+\.\d+", lock["sources"]["mpv"].get("client_api", "")):
        raise ValueError("mpv client API version must be pinned")
    if lock["sources"]["mpv"].get("build_flags") != ["-Djpeg=disabled"]:
        raise ValueError("mpv build must disable optional libjpeg image writer")


def obtain_source(name, source, cache):
    dest = cache / source["archive"]
    if dest.exists():
        actual = sha256(dest)
        if actual != source["sha256"]:
            raise ValueError(f"{name}: cached archive SHA256 {actual} differs from lock {source['sha256']}")
        if "upstream_sha1" in source:
            published = hashlib.sha1()
            with dest.open("rb") as archive:
                for chunk in iter(lambda: archive.read(1024 * 1024), b""):
                    published.update(chunk)
            if published.hexdigest() != source["upstream_sha1"]:
                raise ValueError(f"{name}: cached archive SHA1 differs from published upstream sidecar")
        return dest
    tmp = dest.with_suffix(dest.suffix + ".partial")
    try:
        with urllib.request.urlopen(source["url"], timeout=120) as response, tmp.open("wb") as target:
            digest = hashlib.sha256()
            published = hashlib.sha1() if "upstream_sha1" in source else None
            for chunk in iter(lambda: response.read(1024 * 1024), b""):
                digest.update(chunk)
                if published is not None:
                    published.update(chunk)
                target.write(chunk)
        if digest.hexdigest() != source["sha256"]:
            raise ValueError(f"{name}: downloaded archive SHA256 {digest.hexdigest()} differs from lock {source['sha256']}")
        if published is not None and published.hexdigest() != source["upstream_sha1"]:
            raise ValueError(f"{name}: downloaded archive SHA1 differs from published upstream sidecar")
        tmp.rename(dest)
    finally:
        tmp.unlink(missing_ok=True)
    return dest


def deliverables(prefix):
    if not prefix.is_dir():
        raise ValueError(f"missing build prefix: {prefix}")
    entries = {}
    for path in sorted(prefix.rglob("*")):
        relative = path.relative_to(prefix).as_posix()
        if path.is_symlink():
            entries[relative] = {"type": "symlink", "sha256": hashlib.sha256(os.readlink(path).encode()).hexdigest(), "target": os.readlink(path)}
        elif path.is_file():
            entries[relative] = {"type": "file", "sha256": sha256(path)}
        elif not path.is_dir():
            raise ValueError(f"unexpected non-file deliverable {path}")
    if not entries:
        raise ValueError(f"no deliverables in {prefix}")
    return entries


def compare_deliverables(first, second):
    left, right = deliverables(first), deliverables(second)
    for name in sorted(left.keys() | right.keys()):
        if left.get(name) != right.get(name):
            raise ValueError(f"non-reproducible deliverable {name}: {left.get(name)} != {right.get(name)}")
    return left


def build(output, cache):
    lock = json.loads(LOCK_PATH.read_text())
    check_lock(lock)
    closure = LOCK_PATH.parent / lock["apt_closure"]["file"]
    if sha256(closure) != lock["apt_closure"]["sha256"]:
        raise ValueError("APT closure file SHA256 differs from stack lock")
    if output.exists() and any(output.iterdir()):
        raise ValueError(f"output must be empty: {output}")
    output.mkdir(parents=True, exist_ok=True)
    cache.mkdir(parents=True, exist_ok=True)
    for name, source in lock["sources"].items():
        if name == "qt":
            for archive in source["archives"]:
                component = archive["component"]
                print(f"Verifying official Qt {source['version']} {component} ({archive['sha256']})", flush=True)
                obtain_source(f"Qt {component}", archive, cache)
            continue
        print(f"Verifying {name} {source['version']} ({source['sha256']})", flush=True)
        obtain_source(name, source, cache)
    repository = LOCK_PATH.parent.parent.parent.resolve()
    command = ["podman", "run", "--rm", "--pull=always", "--platform", lock["platform"],
               "--security-opt", "label=disable",
               "--volume", f"{repository}:/workspace:ro", "--volume", f"{cache}:/sources:ro",
               "--volume", f"{output}:/out", "--workdir", "/workspace",
               "--env", f"APT_SNAPSHOT={lock['apt_snapshot']}",
               "--env", f"SOURCE_DATE_EPOCH={lock['source_date_epoch']}",
               "--env", f"MPV_CLIENT_API={lock['sources']['mpv']['client_api']}",
               "--env", f"MPV_JPEG_FLAG={lock['sources']['mpv']['build_flags'][0]}",
               "--env", f"APT_CLOSURE_SHA256={lock['apt_closure']['sha256']}",
               "--env", f"APT_INSTALLED_SHA256={lock['apt_closure']['installed_sha256']}",
               "--env", f"APT_DEBS_SHA256={lock['apt_closure']['downloaded_debs_sha256']}",
               lock["container"], "bash", "packaging/media/container-build.sh"]
    with (output / "build.log").open("w") as transcript:
        result = subprocess.run(command, stdout=transcript, stderr=subprocess.STDOUT, check=False)
    if result.returncode:
        raise RuntimeError(f"container exited {result.returncode}; see {output / 'build.log'}")
    prefix = output / "prefix"
    manifest = {"schema": 1, "lock_sha256": sha256(LOCK_PATH), "container": lock["container"],
                "apt_snapshot": lock["apt_snapshot"], "sources": lock["sources"],
                "recipe_sha256": {name: sha256(LOCK_PATH.parent / name) for name in
                                  ("build.py", "container-build.sh", "build-device-check.sh",
                                   "build-qt-kit.py", "build-apt-closure.json")},
                "artifacts": deliverables(prefix)}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"Built {prefix} ({len(manifest['artifacts'])} files/symlinks); manifest {output / 'manifest.json'}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="new output directory; prefix and configuration records written here")
    parser.add_argument("--source-cache", type=Path, help="optional shared verified download cache")
    args = parser.parse_args()
    try:
        build(args.output.resolve(), (args.source_cache or args.output / "sources").resolve())
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
