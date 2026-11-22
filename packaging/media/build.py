#!/usr/bin/env python3
"""Download immutable sources, then build in a rootless Ubuntu container."""

import argparse
import hashlib
import importlib.util
import json
import os
import re
import subprocess
import sys
import urllib.request
from pathlib import Path

LOCK_PATH = Path(__file__).with_name("stack.lock.json")
REQUIRED = {"ca_certificates", "mpv", "libplacebo", "vulkan_headers", "ffmpeg"}
PATCH_SPEC = importlib.util.spec_from_file_location("furami_media_source_patches", LOCK_PATH.with_name("source-patches.py"))
source_patches = importlib.util.module_from_spec(PATCH_SPEC)
PATCH_SPEC.loader.exec_module(source_patches)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def check_lock(lock):
    if lock.get("schema") != 1 or set(lock.get("sources", {})) != REQUIRED:
        raise ValueError("expected schema 1 and five frozen media sources")
    source_patches.entries(lock["sources"])
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
        if not re.fullmatch(r"[0-9a-f]{64}", source.get("sha256", "")):
            raise ValueError(f"{name}: missing or invalid SHA256")
        if not source.get("url", "").startswith("https://") or not re.fullmatch(r"[a-z0-9-]+\.(tar\.(gz|xz)|deb)", source.get("archive", "")):
            raise ValueError(f"{name}: immutable HTTPS source URL and filename required")
        if not source.get("version"):
            raise ValueError(f"{name}: version required")
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
        return dest
    tmp = dest.with_suffix(dest.suffix + ".partial")
    try:
        with urllib.request.urlopen(source["url"], timeout=120) as response, tmp.open("wb") as target:
            digest = hashlib.sha256()
            for chunk in iter(lambda: response.read(1024 * 1024), b""):
                digest.update(chunk)
                target.write(chunk)
        if digest.hexdigest() != source["sha256"]:
            raise ValueError(f"{name}: downloaded archive SHA256 {digest.hexdigest()} differs from lock {source['sha256']}")
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
    lock_bytes = LOCK_PATH.read_bytes()
    lock = json.loads(lock_bytes)
    check_lock(lock)
    patch_inputs = source_patches.inputs(LOCK_PATH.parent, lock["sources"])
    lock_sha256 = hashlib.sha256(lock_bytes).hexdigest()
    recipes = {name: sha256(LOCK_PATH.parent / name) for name in
               ("build.py", "container-build.sh", "build-device-check.sh",
                "build-apt-closure.json", "source-patches.py")}
    closure = LOCK_PATH.parent / lock["apt_closure"]["file"]
    if sha256(closure) != lock["apt_closure"]["sha256"]:
        raise ValueError("APT closure file SHA256 differs from stack lock")
    if output.exists() and any(output.iterdir()):
        raise ValueError(f"output must be empty: {output}")
    output.mkdir(parents=True, exist_ok=True)
    source_patches.snapshot(output / "source-inputs", lock_bytes, patch_inputs)
    cache.mkdir(parents=True, exist_ok=True)
    for name, source in lock["sources"].items():
        print(f"Verifying {name} {source['version']} ({source['sha256']})", flush=True)
        obtain_source(name, source, cache)
    repository = LOCK_PATH.parent.parent.parent.resolve()
    command = ["podman", "run", "--rm", "--pull=always", "--platform", lock["platform"],
               "--security-opt", "label=disable",
               "--volume", f"{repository}:/workspace:ro", "--volume", f"{cache}:/sources:ro",
               "--volume", f"{output / 'source-inputs'}:/media-inputs:ro",
               "--volume", f"{output}:/out", "--workdir", "/workspace",
               "--env", f"APT_SNAPSHOT={lock['apt_snapshot']}",
               "--env", f"SOURCE_DATE_EPOCH={lock['source_date_epoch']}",
               "--env", f"FURAMI_MEDIA_LOCK_SHA256={lock_sha256}",
               "--env", f"MPV_CLIENT_API={lock['sources']['mpv']['client_api']}",
               "--env", f"MPV_JPEG_FLAG={lock['sources']['mpv']['build_flags'][0]}",
               "--env", f"APT_CLOSURE_SHA256={lock['apt_closure']['sha256']}",
               "--env", f"APT_INSTALLED_SHA256={lock['apt_closure']['installed_sha256']}",
               "--env", f"APT_DEBS_SHA256={lock['apt_closure']['downloaded_debs_sha256']}",
               "--env", f"FURAMI_BUILD_JOBS={os.environ.get('FURAMI_BUILD_JOBS', '2')}",
               lock["container"], "bash", "packaging/media/container-build.sh"]
    with (output / "build.log").open("w") as transcript:
        result = subprocess.run(command, stdout=transcript, stderr=subprocess.STDOUT, check=False)
    if result.returncode:
        raise RuntimeError(f"container exited {result.returncode}; see {output / 'build.log'}")
    prefix = output / "prefix"
    if (LOCK_PATH.read_bytes() != lock_bytes
            or patch_inputs != source_patches.inputs(LOCK_PATH.parent, lock["sources"])
            or recipes != {name: sha256(LOCK_PATH.parent / name) for name in recipes}):
        raise ValueError("media recipe or source patch inputs changed during build")
    manifest = {"schema": 1, "lock_sha256": lock_sha256, "container": lock["container"],
                "apt_snapshot": lock["apt_snapshot"], "sources": lock["sources"],
                "recipe_sha256": recipes,
                "source_patches": {"path": "source-patches.json",
                                   "sha256": sha256(output / "source-patches.json")},
                "artifacts": deliverables(prefix)}
    source_patches.verify_provider(output, manifest, lock["sources"])
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
