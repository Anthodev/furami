#!/usr/bin/env python3
"""Internal extracted-tree and ELF inspection helpers for the AppImage audit.

Hashes observed files, models bundled dependency search contexts and records
static libplacebo link provenance. The supported command-line entry is audit.py.
Dependency metadata comes from the complete extracted tree; host ABI policy
belongs to the caller. Loader search remains an approximation, not runtime proof.
"""

from __future__ import annotations

from collections import deque
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
from pathlib import Path
from typing import Any

NEEDED_RE = re.compile(
    r"^\s*(?:0x[0-9a-f]+\s+)?\((NEEDED|SONAME|RPATH|RUNPATH)\)\s+"
    r"(?:Shared library|Library soname|Library rpath|Library runpath):\s+\[(.*)\]"
)
HEADER_RE = re.compile(r"^\s+(Class|Machine|Type):\s+(.+?)\s*$")


def run_cmd(argv: list[str], timeout: float) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        capture_output=True,
        text=True,
        timeout=timeout,
        env={**os.environ, "LC_ALL": "C"},
    )


def sha256_file(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def is_elf(path: str) -> bool:
    try:
        with open(path, "rb") as handle:
            return handle.read(4) == b"\x7fELF"
    except OSError:
        return False


def walk_tree(root: str) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """Collect regular files (hashed) and symlinks (not followed)."""
    files: list[dict[str, Any]] = []
    symlinks: list[dict[str, Any]] = []
    root_real = os.path.realpath(root)
    for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
        dirnames.sort()
        for dirname in sorted(dirnames):
            abs_dir = os.path.join(dirpath, dirname)
            if os.path.islink(abs_dir):
                target = os.readlink(abs_dir)
                real_target = os.path.realpath(abs_dir)
                inside = real_target.startswith(root_real + os.sep)
                symlinks.append(
                    {
                        "path": os.path.relpath(abs_dir, root),
                        "target": target,
                        "target_absolute": os.path.isabs(target),
                        "resolved_within_root": inside,
                        "resolved_target": os.path.relpath(real_target, root_real)
                        if inside
                        else real_target,
                        "is_directory": True,
                    }
                )
        for name in sorted(filenames):
            abs_path = os.path.join(dirpath, name)
            rel_path = os.path.relpath(abs_path, root)
            if os.path.islink(abs_path):
                target = os.readlink(abs_path)
                real_target = os.path.realpath(abs_path)
                inside = real_target.startswith(root_real + os.sep)
                symlinks.append(
                    {
                        "path": rel_path,
                        "target": target,
                        "target_absolute": os.path.isabs(target),
                        "resolved_within_root": inside,
                        "resolved_target": os.path.relpath(real_target, root_real)
                        if inside
                        else real_target,
                    }
                )
                continue
            if not os.path.isfile(abs_path):
                continue
            try:
                stat = os.lstat(abs_path)
                files.append(
                    {
                        "path": rel_path,
                        "size": stat.st_size,
                        "mode": oct(stat.st_mode & 0o777),
                        "sha256": sha256_file(abs_path),
                    }
                )
            except OSError as exc:
                files.append({"path": rel_path, "error": f"unreadable: {exc}"})
    return files, symlinks


def detect_elf_files(root: str, files: list[dict[str, Any]]) -> list[str]:
    elf_paths: list[str] = []
    for entry in files:
        if "error" in entry:
            continue
        abs_path = os.path.join(root, entry["path"])
        if is_elf(abs_path):
            elf_paths.append(entry["path"])
    return elf_paths


def parse_readelf_output(output: str) -> dict[str, Any]:
    header: dict[str, str] = {}
    soname: str | None = None
    needed: list[str] = []
    rpath: str | None = None
    runpath: str | None = None
    in_dynamic = False
    for line in output.splitlines():
        header_match = HEADER_RE.match(line)
        if header_match and not in_dynamic:
            header[header_match.group(1).lower()] = header_match.group(2)
        lowered = line.lower()
        if lowered.startswith("dynamic section at offset") or lowered.startswith(
            "there is no dynamic section"
        ):
            in_dynamic = True
            continue
        if in_dynamic:
            needed_match = NEEDED_RE.match(line)
            if needed_match:
                kind, value = needed_match.group(1), needed_match.group(2).strip()
                if kind == "NEEDED":
                    needed.append(value)
                elif kind == "SONAME":
                    soname = value
                elif kind == "RPATH":
                    rpath = value
                elif kind == "RUNPATH":
                    runpath = value
    return {
        "class": header.get("class"),
        "machine": header.get("machine"),
        "elf_type": header.get("type"),
        "soname": soname,
        "needed": needed,
        "rpath": rpath,
        "runpath": runpath,
    }


def read_elf_metadata(
    root: str, elf_paths: list[str], readelf: str
) -> tuple[list[dict[str, Any]], list[str]]:
    """readelf -h -d per ELF file. Returns (entries, warnings)."""
    entries: list[dict[str, Any]] = []
    warnings: list[str] = []
    for rel_path in elf_paths:
        abs_path = os.path.join(root, rel_path)
        entry: dict[str, Any] = {
            "path": rel_path,
            "class": None,
            "machine": None,
            "elf_type": None,
            "soname": None,
            "needed": [],
            "rpath": None,
            "runpath": None,
            "readelf_error": None,
        }
        try:
            proc = run_cmd([readelf, "-h", "-d", "--", abs_path], timeout=15)
            if proc.returncode != 0:
                entry["readelf_error"] = proc.stderr.strip().splitlines()[0] if proc.stderr.strip() else f"readelf exited {proc.returncode}"
                warnings.append(f"readelf failed on {rel_path}: {entry['readelf_error']}")
            else:
                entry.update(parse_readelf_output(proc.stdout))
        except (subprocess.TimeoutExpired, OSError) as exc:
            entry["readelf_error"] = str(exc)
            warnings.append(f"readelf failed on {rel_path}: {exc}")
        entries.append(entry)
    return entries, warnings


def expand_runpath(
    runpath: str | None, requester_abs: str, root_real: str, platform_dir: str
) -> list[str]:
    """Expand a DT_RUNPATH string into absolute candidate directories inside root."""
    if not runpath:
        return []
    requester_dir = os.path.dirname(requester_abs)
    candidates: list[str] = []
    for token in runpath.split(":"):
        token = token.strip()
        if not token:
            continue
        expanded = (
            token.replace("$ORIGIN", requester_dir)
            .replace("${ORIGIN}", requester_dir)
            # $LIB depends on target loader architecture/multiarch configuration;
            # leave it unresolved rather than guessing "lib" or "lib64".
            .replace("$PLATFORM", platform_dir)
            .replace("${PLATFORM}", platform_dir)
        )
        if "$" in expanded:
            continue  # unresolvable variable: recorded as data, skipped as candidate
        expanded = os.path.normpath(expanded)
        if expanded.startswith(root_real + os.sep):
            candidates.append(expanded)
    return candidates


def build_shipped_index(
    root: str, elf_entries: list[dict[str, Any]], symlinks: list[dict[str, Any]]
) -> dict[str, dict[str, str]]:
    """Index loader-visible names by directory, preserving symlink and target."""
    root_real = os.path.realpath(root)
    elf_targets = {entry["path"] for entry in elf_entries if not entry.get("readelf_error")}
    index: dict[str, dict[str, str]] = {}
    for path in elf_targets:
        basename = os.path.basename(path)
        if basename.startswith("lib") and ".so" in basename:
            index[path] = {"path": path, "resolved_target": path}
    for link in symlinks:
        path = link["path"]
        if not link["resolved_within_root"] or not (
            os.path.basename(path).startswith("lib") and ".so" in os.path.basename(path)
        ):
            continue
        target = link["resolved_target"]
        if target in elf_targets and os.path.isfile(os.path.join(root_real, target)):
            index[path] = {"path": path, "resolved_target": target}
    return index


def resolve_dependency_closure(
    root: str,
    elf_entries: list[dict[str, Any]],
    file_hashes: dict[str, str],
    symlinks: list[dict[str, Any]],
    library_paths: list[str],
    warnings: list[str],
) -> list[dict[str, Any]]:
    """Resolve every requester in its loader context, not by global SONAME.

    Explicit paths model launch LD_LIBRARY_PATH. RUNPATH applies only to direct
    dependencies; RPATH (when RUNPATH absent) is inherited by descendants.
    elf_entries must contain metadata for every inspected ELF in the tree.
    Dependencies absent from its shipped index remain missing, even if installed
    on the audit host.
    This is an approximation, not evidence of dlopen or runtime environment.
    """
    root_real = os.path.realpath(root)
    platform_dir = platform.machine()
    shipped = build_shipped_index(root, elf_entries, symlinks)
    metadata: dict[str, dict[str, Any]] = {e["path"]: e for e in elf_entries}
    explicit_dirs: list[str] = []
    for path in library_paths:
        candidate = os.path.realpath(os.path.join(root_real, path))
        if not (candidate == root_real or candidate.startswith(root_real + os.sep)):
            warnings.append(f"library search path escapes staged root: {path}")
        elif os.path.isdir(candidate):
            explicit_dirs.append(candidate)
        else:
            warnings.append(f"library search path absent: {path}")
    queue = deque(
        (entry["path"], name, ())
        for entry in elf_entries for name in entry["needed"]
    )
    entries: list[dict[str, Any]] = []
    visited: set[tuple[str, str, tuple[str, ...]]] = set()
    max_entries = 5000

    while queue:
        requester, name, inherited = queue.popleft()
        key = (requester, name, inherited)
        if key in visited:
            continue
        visited.add(key)
        if len(entries) >= max_entries:
            warnings.append("dependency closure exceeded 5000 requester/name contexts")
            entries.append({"name": name, "requester": requester, "requested_by": [requester],
                            "resolution": {"kind": "unresolved-truncated", "path": None}})
            break
        source = metadata.get(requester)
        if source is None:
            warnings.append(f"requester metadata unavailable: {requester}")
            continue
        requester_abs = os.path.join(root_real, requester)
        for tag in ("runpath", "rpath"):
            value = source.get(tag)
            if value and ("$LIB" in value or "${LIB}" in value):
                warning = f"{requester}: {tag} uses loader-specific $LIB; cannot verify from staged tree"
                if warning not in warnings:
                    warnings.append(warning)
        own_rpath = expand_runpath(
            source.get("rpath") if not source.get("runpath") else None,
            requester_abs, root_real, platform_dir
        )
        inherited_next = tuple(dict.fromkeys((*own_rpath, *inherited)))
        runpath = expand_runpath(source.get("runpath"), requester_abs, root_real, platform_dir)
        search_dirs = list(dict.fromkeys(
            (*inherited_next, *explicit_dirs, *runpath)
            if own_rpath or inherited else (*explicit_dirs, *runpath)
        ))
        # Explicit paths precede DT_RUNPATH; DT_RPATH precedes explicit paths.
        resolution: dict[str, Any] | None = None
        for directory in search_dirs:
            candidate = os.path.join(directory, name)
            if os.path.isfile(candidate):
                relative = os.path.relpath(candidate, root_real)
                linked = shipped.get(relative)
                if linked:
                    target = linked["resolved_target"]
                    resolution = {"kind": "bundled", "path": relative,
                                  "resolved_target": target, "file_sha256": file_hashes[target],
                                  "host_package": None}
                    break
        if resolution is None:
            resolution = {"kind": "missing", "path": None, "file_sha256": None,
                          "host_package": None}
            warnings.append(f"missing dependency: {name} (required by {requester})")
        if resolution["kind"] != "bundled":
            candidates = sorted(path for path in shipped if os.path.basename(path) == name)
            if candidates:
                resolution["unsearched_staged_candidates"] = candidates
                warnings.append(
                    f"staged dependency not in loader search paths: {name} "
                    f"(required by {requester}; candidates {candidates})"
                )
        resolution["loader_verified"] = False
        entries.append({"name": name, "requester": requester, "requested_by": [requester],
                        "search_dirs": [os.path.relpath(p, root_real) for p in search_dirs],
                        "resolution": resolution})
        if resolution["kind"] == "bundled":
            child = resolution["resolved_target"]
            for needed in metadata[child]["needed"]:
                queue.append((child, needed, inherited_next))
    return entries


STATIC_ARCHIVE_OWNERS = {
    "glslang-dev": {"libSPIRV.a", "libglslang.a", "libMachineIndependent.a",
                    "libOSDependent.a", "libGenericCodeGen.a",
                    "libglslang-default-resource-limits.a"},
    "spirv-tools": {"libSPIRV-Tools.a", "libSPIRV-Tools-opt.a"},
}

STATIC_SOURCE_FACTS = {
    "glslang-dev": {
        "version": "15.1.0-2~ubuntu0.24.04.2",
        "source_package": "glslang",
        "source_descriptor": "https://archive.ubuntu.com/ubuntu/pool/universe/g/glslang/glslang_15.1.0-2~ubuntu0.24.04.2.dsc",
        "upstream_source_sha256": "4bdcd8cdb330313f0d4deed7be527b0ac1c115ff272e492853a6e98add61b4bc",
        "ubuntu_patch_sha256": "4a1421f9f57bcf573236dfbdee520db44b9a55d2dd9935773b09fe192ca2d5e4",
        "spdx_scope": "source-level BSD-3-Clause, MIT, GPL-3.0-or-later, LicenseRef-NVIDIA-Apple-MIT; linked files not established",
        "notices": "BSD copyright/license/disclaimer; MIT Khronos notices; GPL-3 for generated parser if linked; NVIDIA custom notice/disclaimer if preprocessor linked",
        "license_source": "https://changelogs.ubuntu.com/changelogs/pool/universe/g/glslang/glslang_15.1.0-2~ubuntu0.24.04.2/copyright",
    },
    "spirv-tools": {
        "version": "2025.1~rc1-1~ubuntu0.24.04.2",
        "source_package": "spirv-tools",
        "source_descriptor": "https://archive.ubuntu.com/ubuntu/pool/universe/s/spirv-tools/spirv-tools_2025.1~rc1-1~ubuntu0.24.04.2.dsc",
        "upstream_source_sha256": "6895160d5a842552eda6412440747cbadddd47005957bc43bb03926162686777",
        "ubuntu_patch_sha256": "0f26aefbeedb14bc79682cc8502a762c8fff920b0a05e3b12c27fead44e805b5",
        "spdx_scope": "Apache-2.0",
        "notices": "Apache-2.0 text and upstream copyright/attribution; upstream NOTICE if present in distributed source",
        "license_source": "https://changelogs.ubuntu.com/changelogs/pool/universe/s/spirv-tools/spirv-tools_2025.1~rc1-1~ubuntu0.24.04.2/copyright",
    },
}

STATIC_LINK_FLAGS = {
    "-lglslang-default-resource-limits": {
        "archive": "libglslang-default-resource-limits.a",
        "owner_package": "glslang-dev",
        "version": "15.1.0-2~ubuntu0.24.04.2",
        "filelist_url": "https://packages.ubuntu.com/noble-updates/amd64/glslang-dev/filelist",
    },
}


def collect_static_components(
    root: str, build_ninja: str | None, pc_override: str | None,
    warnings: list[str],
) -> dict[str, Any]:
    """Correlate actual libplacebo linker args with pinned Ubuntu package hashes."""
    result: dict[str, Any] = {"status": "not-audited", "build_ninja": build_ninja,
                              "pkg_config": None, "components": [], "unowned_archives": []}
    pc = pc_override or os.path.join(root, "lib/pkgconfig/libplacebo.pc")
    if not os.path.isfile(pc) and not pc_override:
        pc = os.path.join(root, "usr/lib/pkgconfig/libplacebo.pc")
    if not os.path.isfile(pc):
        if any(Path(root).rglob("libplacebo.so.*")):
            warnings.append("static libplacebo ownership unverified: pkg-config metadata unavailable")
        return result
    result["pkg_config"] = pc
    if not build_ninja or not os.path.isfile(build_ninja):
        warnings.append("static libplacebo ownership unverified: provide --build-ninja from same build")
        return result
    try:
        ninja = Path(build_ninja).read_text(encoding="utf-8")
        pc_text = Path(pc).read_text(encoding="utf-8")
        closure = json.loads((Path(__file__).parents[1] / "media/build-apt-closure.json").read_text())
        result["build_ninja_sha256"] = sha256_file(build_ninja)
        result["pkg_config_sha256"] = sha256_file(pc)
    except (OSError, ValueError) as exc:
        warnings.append(f"static link metadata unreadable: {exc}")
        return result
    link_lines = re.findall(r"^ LINK_ARGS = (.*)$", ninja, flags=re.MULTILINE)
    libplacebo_links = [line for line in link_lines if "libplacebo.so.360" in line]
    pkg_lines = re.findall(r"^Libs.private: (.*)$", pc_text, flags=re.MULTILINE)
    if len(libplacebo_links) != 1 or len(pkg_lines) != 1:
        warnings.append("static libplacebo link not uniquely identified in build.ninja/pkg-config")
        return result
    packages = {item["package"]: item["version"] for item in closure["installed"]}
    debs = {item["filename"]: item["sha256"] for item in closure["downloaded_debs"]}
    link_flags = {token for token in libplacebo_links[0].split() if token.startswith("-lglslang-")}
    pc_flags = {token for token in pkg_lines[0].split() if token.startswith("-lglslang-")}
    if link_flags != pc_flags:
        warnings.append(f"libplacebo linker flag mismatch: build.ninja {sorted(link_flags)} vs pkg-config {sorted(pc_flags)}")
        return result
    search_overrides = [token for token in libplacebo_links[0].split() if token.startswith("-L")]
    shared_glslang = False
    output_checked = False
    linked_image = None
    for relpath in ("lib/libplacebo.so.360", "usr/lib/libplacebo.so.360"):
        candidate = os.path.join(root, relpath)
        if is_elf(candidate):
            linked_image = candidate
            break
    readelf_tool = shutil.which("readelf")
    if linked_image and readelf_tool:
        try:
            dynamic = run_cmd([readelf_tool, "-d", "--", linked_image], timeout=15)
            if dynamic.returncode == 0:
                output_checked = True
                shared_glslang = bool(re.search(
                    r"\(NEEDED\).*libglslang-default-resource-limits\.so", dynamic.stdout
                ))
        except (subprocess.TimeoutExpired, OSError):
            pass
    result["linked_output_needed_checked"] = output_checked
    if search_overrides:
        warnings.append(f"libplacebo alternate -L paths prevent static -l proof: {search_overrides}")
    if shared_glslang:
        warnings.append("libplacebo DT_NEEDED selects glslang default-resource-limits shared object")
    elif linked_image and not output_checked:
        warnings.append("libplacebo DT_NEEDED unavailable; static -l proof incomplete")
    resolved_flags: list[dict[str, str]] = []
    unresolved_flags: list[str] = []
    for flag in sorted(link_flags):
        proof = STATIC_LINK_FLAGS.get(flag)
        if (proof and not search_overrides and not shared_glslang and
                (not linked_image or output_checked) and
                packages.get(proof["owner_package"]) == proof["version"]):
            resolved_flags.append({"flag": flag, **proof})
        else:
            unresolved_flags.append(flag)
    result["resolved_linker_flags"] = resolved_flags
    result["unverified_linker_flags"] = unresolved_flags
    if unresolved_flags:
        warnings.append(f"libplacebo -l flags need static/shared linker proof: {unresolved_flags}")
    def archive_tokens(line: str) -> set[str]:
        return {os.path.basename(token) for token in line.split() if token.endswith(".a")}
    flag_archives = {proof["archive"] for proof in resolved_flags}
    linked = archive_tokens(libplacebo_links[0]) | flag_archives
    recorded = archive_tokens(pkg_lines[0]) | flag_archives
    if linked != recorded:
        warnings.append(f"libplacebo static archive mismatch: build.ninja {sorted(linked)} vs pkg-config {sorted(recorded)}")
        return result
    apt_output = Path(build_ninja).resolve().parents[2]
    actual_installed = apt_output / "apt-installed.tsv"
    actual_debs = apt_output / "apt-debs.sha256"
    apt_logs_available = actual_installed.is_file() and actual_debs.is_file()
    if not apt_logs_available:
        warnings.append("actual build APT package/version/hash logs unavailable; using pinned APT metadata only")
    else:
        installed_lines = set(actual_installed.read_text(encoding="utf-8").splitlines())
        deb_lines = actual_debs.read_text(encoding="utf-8").splitlines()
    ownership_verified = apt_logs_available
    for package, archives in STATIC_ARCHIVE_OWNERS.items():
        used = sorted(linked & archives)
        if not used:
            continue
        version = packages.get(package)
        filename = f"{package}_{version}_amd64.deb"
        digest = debs.get(filename)
        if apt_logs_available and (
            f"{package}\t{version}\tamd64" not in installed_lines
            or not any(
                line.startswith(f"{digest}  ") and line.endswith("/" + filename)
                for line in deb_lines
            )
        ):
            warnings.append(f"actual build APT identity mismatch for {package}")
            ownership_verified = False
        if not digest:
            warnings.append(f"pinned APT ownership incomplete for {package}: {filename}")
            ownership_verified = False
        facts = STATIC_SOURCE_FACTS[package]
        if version != facts["version"]:
            warnings.append(f"static source metadata version mismatch: {package} {version}")
            ownership_verified = False
        result["components"].append({
            "package": package, "version": version, "archive_names": used,
            "deb_filename": filename, "deb_sha256": digest,
            "checksum_scope": "downloaded Ubuntu binary .deb; not upstream source tarball",
            "source": facts if version == facts["version"] else None,
        })
    result["unowned_archives"] = sorted(linked - set().union(*STATIC_ARCHIVE_OWNERS.values()))
    if result["unowned_archives"]:
        warnings.append(f"unowned libplacebo static archives: {result['unowned_archives']}")
    result["status"] = (
        "link-inputs-matched-objects-unverified"
        if ownership_verified and not result["unowned_archives"] and not unresolved_flags
        else "incomplete"
    )
    return result
