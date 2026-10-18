#!/usr/bin/env python3
"""Verify, snapshot and apply only the frozen FFmpeg source patches."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


HEX = re.compile(r"[0-9a-f]{64}\Z")


def entries(sources):
    """Return ordered patches, rejecting unknown patchable components and paths."""
    result = []
    seen = set()
    for name, source in sources.items():
        if "patches" not in source:
            continue
        if name != "ffmpeg" or not isinstance(source["patches"], list):
            raise ValueError("only FFmpeg supports an ordered source patches array")
        for entry in source["patches"]:
            if (not isinstance(entry, dict) or set(entry) != {"file", "sha256"}
                    or not isinstance(entry["file"], str)
                    or not re.fullmatch(r"patches/(?:[A-Za-z0-9_-][A-Za-z0-9_.-]*/)*[A-Za-z0-9_-][A-Za-z0-9_.-]*\.patch", entry["file"])
                    or any(part in (".", "..") for part in entry["file"].split("/"))
                    or not isinstance(entry["sha256"], str) or not HEX.fullmatch(entry["sha256"])
                    or entry["file"] in seen):
                raise ValueError("invalid or duplicate FFmpeg source patch path/SHA256")
            seen.add(entry["file"])
            result.append({"source": name, **entry})
    return result


def read_verified(root, relative, expected):
    """Hash the same captured bytes that consumers use, without following symlinks."""
    root = Path(root)
    if (not isinstance(relative, str) or not relative or relative.startswith("/")
            or "\\" in relative or "\0" in relative
            or any(part in ("", ".", "..") for part in relative.split("/"))):
        raise ValueError("unsafe source input path")
    path = root / relative
    if (any(parent.is_symlink() for parent in (path, *path.parents))
            or not path.is_file() or not path.resolve().is_relative_to(root.resolve())):
        raise ValueError(f"source input must be a confined regular nonsymlink file: {relative}")
    data = path.read_bytes()
    if hashlib.sha256(data).hexdigest() != expected:
        raise ValueError(f"source input SHA256 differs from lock: {relative}")
    return data


def inputs(root, sources):
    return {row["file"]: read_verified(root, row["file"], row["sha256"])
            for row in entries(sources)}


def snapshot(destination, lock_bytes, patch_inputs):
    destination.mkdir()
    for relative, data in {"stack.lock.json": lock_bytes, **patch_inputs}.items():
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open("xb") as stream:
            stream.write(data)
        target.chmod(0o444)


def receipt(sources):
    return {"schema": 1, "patches": entries(sources)}


def apply(snapshot_root, ffmpeg_source, receipt_path, lock_sha256):
    lock = json.loads(read_verified(snapshot_root, "stack.lock.json", lock_sha256))
    patch_inputs = inputs(snapshot_root, lock["sources"])
    if (not ffmpeg_source.is_dir() or ffmpeg_source.is_symlink()
            or any(path.is_symlink() for path in ffmpeg_source.parents)):
        raise ValueError("FFmpeg source must be a regular nonsymlink directory")
    if receipt_path.exists() or receipt_path.is_symlink():
        raise ValueError("source patch receipt must be new")
    environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    environment.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
                       GIT_CEILING_DIRECTORIES=str(ffmpeg_source.resolve().parent))
    # Check and apply identical, already-verified bytes, never the mutable recipe.
    # git apply refuses paths outside its working tree and never executes hooks.
    for data in patch_inputs.values():
        command = ["git", "apply", "--whitespace=error", "-"]
        subprocess.run([*command[:2], "--check", *command[2:]], input=data,
                       cwd=ffmpeg_source, env=environment, check=True)
        subprocess.run(command, input=data, cwd=ffmpeg_source, env=environment, check=True)
    receipt_path.write_text(json.dumps(receipt(lock["sources"]), indent=2, sort_keys=True) + "\n")


def verify_provider(media, manifest, sources):
    """Require the applied-patch receipt and all frozen patch inputs to match."""
    expected = receipt(sources)
    if not expected["patches"]:
        return
    record = manifest.get("source_patches", {})
    if (not isinstance(record, dict) or set(record) != {"path", "sha256"}
            or record.get("path") != "source-patches.json"
            or not isinstance(record.get("sha256"), str) or not HEX.fullmatch(record["sha256"])):
        raise ValueError("media applied source patch receipt missing or malformed")
    actual = json.loads(read_verified(media, record["path"], record["sha256"]))
    if actual != expected:
        raise ValueError("media applied source patches differ from stack lock")
    read_verified(media / "source-inputs", "stack.lock.json", manifest["lock_sha256"])
    inputs(media / "source-inputs", sources)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("snapshot", type=Path)
    parser.add_argument("ffmpeg_source", type=Path)
    parser.add_argument("receipt", type=Path)
    parser.add_argument("lock_sha256")
    arguments = parser.parse_args()
    apply(arguments.snapshot, arguments.ffmpeg_source, arguments.receipt, arguments.lock_sha256)
