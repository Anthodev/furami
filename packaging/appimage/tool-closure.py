#!/usr/bin/env python3
"""Verify packaging-only Ubuntu snapshot packages after, never during, media APT gate."""
import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
LOCK_PATH = HERE / "tool-closure.lock.json"
MEDIA_LOCK = REPO / "packaging/media/stack.lock.json"
MEDIA_APT = REPO / "packaging/media/build-apt-closure.json"
ARCHIVES = Path("/var/cache/apt/archives")


def sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def locks():
    tool = json.loads(LOCK_PATH.read_text())
    media = json.loads(MEDIA_LOCK.read_text())
    if (tool.get("schema") != 1 or tool.get("media_lock_sha256") != sha(MEDIA_LOCK)
            or tool.get("container") != media["container"]
            or tool.get("apt_snapshot") != media["apt_snapshot"]
            or set(tool.get("packages", {})) != {"file", "libmagic1t64", "libmagic-mgc"}
            or set(tool.get("bundled_appimagetool_executables", {})) != {
                "appimagetool", "mksquashfs", "desktop-file-validate", "zsyncmake"}):
        raise ValueError("stage-only package lock differs from immutable Ubuntu/media identity")
    for package, entry in tool["packages"].items():
        if (entry["architecture"] != "amd64" or entry["version"] != "1:5.45-3build1"
                or entry["deb_filename"] != f"{package}_1%3a5.45-3build1_amd64.deb"
                or not re.fullmatch(r"[0-9a-f]{64}", entry["deb_sha256"])):
            raise ValueError(f"unfrozen stage-only package {package}")
    if sha(MEDIA_APT) != media["apt_closure"]["sha256"]:
        raise ValueError("media APT closure bytes differ from signed snapshot lock")
    return tool, json.loads(MEDIA_APT.read_text())


def downloaded(tool, media, archive_dir):
    expected = {entry["deb_filename"]: entry["deb_sha256"] for entry in tool["packages"].values()}
    prior = {entry["filename"] for entry in media["downloaded_debs"]}
    actual = {path.name: path for path in archive_dir.glob("*.deb")}
    if set(actual) - prior != set(expected):
        raise ValueError(f"stage-only package downloads differ: expected {sorted(expected)}, got {sorted(set(actual) - prior)}")
    for name, expected_hash in expected.items():
        if sha(actual[name]) != expected_hash:
            raise ValueError(f"stage-only signed snapshot package SHA256 mismatch: {name}")
    return {name: expected[name] for name in sorted(expected)}


def installed(tool, media_rows, package_rows, root=Path("/")):
    expected = {tuple(line.split("\t")) for line in media_rows.splitlines() if line}
    expected |= {(name, entry["version"], entry["architecture"])
                 for name, entry in tool["packages"].items()}
    actual = {tuple(line.split("\t")) for line in package_rows.splitlines() if line}
    if actual != expected:
        raise ValueError(f"stage-only package installation changed media closure: missing={sorted(expected - actual)}, extra={sorted(actual - expected)}")
    records = {}
    for name, entry in tool["packages"].items():
        hashes = {}
        for path, expected_hash in entry["files_sha256"].items():
            item = root / path.lstrip("/")
            if not item.is_file() or sha(item) != expected_hash:
                raise ValueError(f"stage-only tool bytes mismatch: {path}")
            hashes[path] = expected_hash
        notice = root / "usr/share/doc" / name / "copyright"
        if not notice.is_file() or sha(notice) != entry["copyright_sha256"]:
            raise ValueError(f"stage-only copyright bytes mismatch: {name}")
        records[name] = {"version": entry["version"], "architecture": entry["architecture"],
                         "deb_filename": entry["deb_filename"], "deb_sha256": entry["deb_sha256"],
                         "files_sha256": hashes, "copyright_path": str(notice),
                         "copyright_sha256": entry["copyright_sha256"],
                         "distribution": "build-only; not shipped"}
    return records


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", choices=("preflight", "downloaded", "installed"), required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    tool, media = locks()
    if args.phase == "preflight":
        print(f"Pinned packaging-only packages: {', '.join(tool['packages'])}")
        return
    if args.phase == "downloaded":
        print(json.dumps(downloaded(tool, media, ARCHIVES), sort_keys=True))
        return
    if args.output is None:
        raise ValueError("--output required for installed phase")
    observed = subprocess.run(["dpkg-query", "-W", "-f=${Package}\t${Version}\t${Architecture}\n"],
                              check=True, capture_output=True, text=True).stdout
    media_rows = Path("/media/apt-installed.tsv").read_text()
    packages = installed(tool, media_rows, observed)
    probe = subprocess.run(["/usr/bin/file", "--version"], check=True, capture_output=True, text=True)
    if not probe.stdout.startswith("file-5.45\n"):
        raise ValueError(f"stage-only file executable version unexpected: {probe.stdout!r}")
    result = {"schema": 1, "scope": tool["scope"], "lock_sha256": sha(LOCK_PATH),
              "media_apt_closure_sha256": sha(MEDIA_APT), "media_installed_sha256": sha(Path("/media/apt-installed.tsv")),
              "packages": packages, "file_version": probe.stdout.strip(),
              "bundled_tool_hashes_expected": tool["bundled_appimagetool_executables"]}
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        sys.exit(f"stage-only tool closure failed: {error}")
