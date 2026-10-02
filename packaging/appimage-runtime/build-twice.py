#!/usr/bin/env python3
"""Build twice in fresh containers and compare static runtime plus provenance."""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def compare(first, second):
    differences = {}
    for name in ("runtime-x86_64", "manifest.json", "link.map", "apk-installed.txt"):
        left, right = first / name, second / name
        if digest(left) != digest(right):
            a, b = left.read_bytes(), right.read_bytes()
            offset = next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
            differences[name] = {"first_sha256": digest(left), "second_sha256": digest(right),
                                 "first_bytes": len(a), "second_bytes": len(b),
                                 "first_different_byte": offset}
    return differences


def classify_variance(first, second, differences):
    """Accept only Clang's proven temporary-object map/path hash variance."""
    material = dict(differences)
    accepted = {}
    if "link.map" not in differences:
        return material, accepted

    pattern = re.compile(r"/tmp/runtime-[0-9a-f]{6}\.o")
    left = (first / "link.map").read_text()
    right = (second / "link.map").read_text()
    left_paths, right_paths = pattern.findall(left), pattern.findall(right)
    if (not left_paths or not right_paths or len(set(left_paths)) != 1
            or len(set(right_paths)) != 1 or len(left_paths) != len(right_paths)):
        return material, accepted
    normalized_left = pattern.sub("/tmp/runtime-<clang-temp>.o", left)
    normalized_right = pattern.sub("/tmp/runtime-<clang-temp>.o", right)
    if normalized_left != normalized_right or "manifest.json" not in differences:
        return material, accepted

    first_manifest = json.loads((first / "manifest.json").read_text())
    second_manifest = json.loads((second / "manifest.json").read_text())
    for manifest, directory in ((first_manifest, first), (second_manifest, second)):
        if (manifest["files_sha256"]["link.map"] != digest(directory / "link.map")
                or manifest["artifact"]["sha256"] != digest(directory / "runtime-x86_64")):
            return material, accepted
        manifest["files_sha256"]["link.map"] = "<raw map varies only by Clang temp path>"
    if first_manifest != second_manifest:
        return material, accepted

    accepted["link.map"] = {
        "cause": "Clang compile/link generated different temporary runtime object names; all other linker map bytes match",
        "first_temporary_object": left_paths[0], "second_temporary_object": right_paths[0],
        "reference_count_each": len(left_paths),
        "normalized_sha256": hashlib.sha256(normalized_left.encode()).hexdigest(),
    }
    accepted["manifest.json"] = {
        "cause": "only files_sha256/link.map differs; both manifests accurately hash their own raw linker maps",
    }
    del material["link.map"]
    del material["manifest.json"]
    return material, accepted


def main(parent, cache):
    if not parent.is_absolute() or parent.exists():
        raise ValueError("comparison output must be a new absolute directory")
    parent.mkdir(parents=True)
    for name in ("first", "second"):
        cmd = [sys.executable, str(HERE / "build.py"), str(parent / name), "--source-cache", str(cache)]
        with (parent / f"{name}.log").open("w") as log:
            completed = subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT, check=False)
        if completed.returncode:
            raise RuntimeError(f"{name} build failed with exit {completed.returncode}; see {parent / (name + '.log')}")
    first, second = parent / "first", parent / "second"
    differences = compare(first, second)
    material, accepted = classify_variance(first, second, differences)
    if material:
        diagnosis = ("Material runtime/provenance variance remains; raw hashes and first differing offsets "
                     "identify changed files. Inspect raw maps and logs; do not ship.")
    elif accepted:
        diagnosis = ("Runtime binary and all normalized provenance match. Clang changed only its temporary "
                     "compile/link object filename in the raw link map; that raw map hash alone changes the "
                     "manifest. Raw maps and manifests remain unchanged on disk.")
    else:
        diagnosis = "Byte-identical runtime, manifest, link map and installed APK closure"
    result = {"schema": 1, "first": str(first), "second": str(second),
              "repeatable": not material, "differences": differences,
              "accepted_variance": accepted, "material_differences": material,
              "diagnosis": diagnosis}
    (parent / "comparison.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    if material:
        raise RuntimeError(f"runtime builds differ materially; inspect {parent / 'comparison.json'} and build logs")
    print(f"Two independent runtime binaries match: {digest(first / 'runtime-x86_64')}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="new absolute directory for two builds")
    parser.add_argument("--source-cache", type=Path, help="optional shared SHA256-verified download cache")
    args = parser.parse_args()
    try:
        default_cache = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "furami" / "appimage-runtime"
        cache = args.source_cache or default_cache
        main(args.output, cache.resolve())
    except (OSError, ValueError, RuntimeError) as error:
        sys.exit(str(error))
