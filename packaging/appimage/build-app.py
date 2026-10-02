#!/usr/bin/env python3
"""Build Furami once in the frozen media stack's Ubuntu 24 environment."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import posixpath
import re
import shutil
import shlex
import subprocess
import sys
import tomllib
import tarfile
import urllib.request

HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parent.parent
APP_LOCK = HERE / "app.lock.json"
STACK_LOCK = HERE.parent / "media/stack.lock.json"
HEX = re.compile(r"[0-9a-f]{64}\Z")
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def check_sha(path, expected, label):
    actual = sha256(path)
    if actual != expected:
        raise ValueError(f"{label}: SHA256 {actual} differs from pinned {expected}")


def crate_sources(lockfile):
    entries = []
    for package in tomllib.loads(lockfile.read_text())["package"]:
        if package["name"] == "furami" and "source" not in package:
            continue
        if package.get("source") != REGISTRY:
            raise ValueError(f"non-registry or unpinned crate source: {package['name']}")
        checksum = package.get("checksum", "")
        if not HEX.fullmatch(checksum):
            raise ValueError(f"{package['name']}: missing Cargo.lock checksum")
        name, version = package["name"], package["version"]
        archive = f"{name}-{version}.crate"
        entries.append({"name": name, "version": version, "archive": archive,
                        "sha256": checksum, "url": f"https://static.crates.io/crates/{name}/{archive}"})
    if not entries:
        raise ValueError("Cargo.lock has no registry crate dependencies")
    return entries


def verify_artifact(prefix, manifest, relative):
    entry = manifest["artifacts"].get(relative)
    if not entry:
        raise ValueError(f"unrecorded media artifact {relative}")
    path = prefix / relative
    if path.resolve() != prefix.resolve() and prefix.resolve() not in path.resolve().parents:
        raise ValueError(f"{relative}: symlink points outside prefix")
    if entry["type"] == "symlink":
        if not path.is_symlink() or os.readlink(path) != entry["target"]:
            raise ValueError(f"{relative}: symlink differs from media manifest")
        if hashlib.sha256(os.readlink(path).encode()).hexdigest() != entry["sha256"]:
            raise ValueError(f"{relative}: symlink SHA256 differs from media manifest")
    elif entry["type"] == "file":
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"{relative}: missing media artifact")
        check_sha(path, entry["sha256"], relative)
    else:
        raise ValueError(f"{relative}: invalid media artifact type")


def verify_media(media, stack):
    manifest_path = media / "manifest.json"
    manifest = json.loads(manifest_path.read_text())
    if manifest.get("schema") != 1 or manifest.get("lock_sha256") != sha256(STACK_LOCK):
        raise ValueError("media stack lock differs from media manifest")
    if manifest.get("container") != stack["container"] or manifest.get("apt_snapshot") != stack["apt_snapshot"]:
        raise ValueError("media OCI image or APT snapshot differs from stack lock")
    if manifest.get("sources") != stack["sources"]:
        raise ValueError("media source versions differ from stack lock")
    closure = STACK_LOCK.parent / stack["apt_closure"]["file"]
    check_sha(closure, stack["apt_closure"]["sha256"], "media APT closure")
    if manifest.get("recipe_sha256", {}).get("build-apt-closure.json") != sha256(closure):
        raise ValueError("media manifest does not attest the APT closure")
    for name in ("apt-installed.tsv", "apt-debs.sha256", "apt-direct-packages.txt"):
        if not (media / name).is_file():
            raise ValueError(f"media APT evidence missing: {name}")
    check_sha(media / "apt-installed.tsv", stack["apt_closure"]["installed_sha256"], "media installed APT packages")
    check_sha(media / "apt-debs.sha256", stack["apt_closure"]["downloaded_debs_sha256"], "media downloaded .deb closure")
    prefix = media / "prefix"
    if not prefix.is_dir():
        raise ValueError("media prefix missing")
    for relative in manifest["artifacts"]:
        if relative.startswith("/") or ".." in Path(relative).parts:
            raise ValueError(f"invalid media artifact path: {relative}")
        verify_artifact(prefix, manifest, relative)
    for required in ("bin/qmake6", "include/mpv/client.h", "lib/libmpv.so"):
        if required not in manifest["artifacts"]:
            raise ValueError(f"missing frozen media build input: {required}")
    return manifest


def obtain(source, cache):
    dest = cache / source["archive"]
    if dest.exists():
        check_sha(dest, source["sha256"], dest.name)
        return
    temporary = dest.with_name(dest.name + ".partial")
    try:
        request = urllib.request.Request(source["url"], headers={"User-Agent": "furami-app-qualification/1"})
        with urllib.request.urlopen(request, timeout=120) as response, temporary.open("wb") as sink:
            if response.url.split(":", 1)[0] != "https":
                raise ValueError(f"HTTPS downgrade downloading {source['archive']}")
            digest = hashlib.sha256()
            for chunk in iter(lambda: response.read(1024 * 1024), b""):
                sink.write(chunk)
                digest.update(chunk)
        if digest.hexdigest() != source["sha256"]:
            raise ValueError(f"{source['archive']}: downloaded SHA256 {digest.hexdigest()} differs from lock")
        temporary.replace(dest)
    finally:
        temporary.unlink(missing_ok=True)



def generated_files(build):
    target = build / "target"
    return {p.relative_to(target).as_posix(): sha256(p) for p in sorted(target.rglob("*"))
            if p.is_file() and p.suffix in (".h", ".hpp", ".hxx", ".cc", ".cpp", ".cxx")
            and "/out/" in p.as_posix()}



def dependency_roles(metadata):
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    root = metadata["resolve"]["root"]
    selected, pending = {root}, [root]
    while pending:
        for dep in nodes[pending.pop()]["deps"]:
            target = dep["pkg"]
            if any(kind["kind"] != "dev" for kind in dep["dep_kinds"]) and target not in selected:
                selected.add(target)
                pending.append(target)
    runtime, pending = {root}, [root]
    while pending:
        for dep in nodes[pending.pop()]["deps"]:
            target = dep["pkg"]
            is_normal = any(kind["kind"] is None for kind in dep["dep_kinds"])
            is_macro = any("proc-macro" in target_kind["kind"] for target_kind in packages[target]["targets"])
            if is_normal and not is_macro and target not in runtime:
                runtime.add(target)
                pending.append(target)
    return {package: ("shipped-code-candidate" if package in runtime else
                      "build-only" if package in selected else "not-selected")
            for package in packages}


def retained_direct_objects(linker_map, app_lock, apt_closure):
    """Account for compiler-driver CRT inputs visible in the final ELF link map."""
    if apt_closure["apt_snapshot"] != json.loads(STACK_LOCK.read_text())["apt_snapshot"]:
        raise ValueError("CRT package snapshot differs from media closure")
    debs = {entry["filename"]: entry["sha256"] for entry in apt_closure["downloaded_debs"]}
    installed = {entry["package"]: entry["version"] for entry in apt_closure["installed"]}
    known = {}
    for name, package in app_lock["direct_object_packages"].items():
        binary = package["binary_package"]
        filename = binary["archive"]
        if (binary["name"] != name or debs.get(filename) != binary["sha256"] or
                installed.get(name) != binary["version"]):
            raise ValueError(f"{name}: direct object binary package differs from frozen APT closure")
        notice = package["notice"]
        if (debs.get(notice["archive"]) != notice["archive_sha256"]
                or not HEX.fullmatch(notice["sha256"])):
            raise ValueError(f"{name}: direct object notice differs from frozen APT closure")
        if set(package["objects"]) != set(package["source_package"]["object_sources"]):
            raise ValueError(f"{name}: direct objects lack exact source files")
        for path, object_sha in package["objects"].items():
            if not HEX.fullmatch(object_sha) or path in known:
                raise ValueError(f"{name}: invalid or duplicate direct object {path}")
            known[path] = (object_sha, package)

    matched = {}
    for line in linker_map.splitlines():
        # Archive members (including libgcc.a(foo.o)) are not direct .o inputs.
        match = re.search(r"\s(/[^():\s]+\.o):\(([^)]*)\)", line)
        if not match:
            continue
        map_path, section = match.groups()
        if map_path.startswith("/build/target/"):
            continue  # Furami/CXX-Qt output, not distribution CRT.
        path = posixpath.normpath(map_path)
        if path not in known:
            raise ValueError(f"unattributed linker object {map_path}")
        parts = line.split()
        size = int(parts[2], 16)
        if path not in matched:
            object_sha, package = known[path]
            matched[path] = {"path": path, "map_path": map_path, "sha256": object_sha,
                             "binary_package": package["binary_package"],
                             "source_package": package["source_package"],
                             "source_path": package["source_package"]["object_sources"][path],
                             "license_expression": package["license_expression"],
                             "license_exception": package.get("license_exception"),
                             "notice": package["notice"], "retained_bytes": 0,
                             "retained_sections": []}
        matched[path]["retained_bytes"] += size
        matched[path]["retained_sections"].append({"name": section, "size": size})
    if not matched:
        raise ValueError("no retained direct objects in final linker map")
    if set(matched) != set(known):
        raise ValueError(f"pinned direct objects not present in final linker map: {sorted(set(known) - set(matched))}")
    return [matched[path] for path in sorted(matched)]


def rust_std_closure(output, linked_archives, app_lock, cache=None):
    """Map linked toolchain rlibs to source crates, without labeling vendor code MIT-only."""
    source = app_lock["toolchain"]["rust-src"]
    standard = app_lock["toolchain"]["rust-std"]
    root = output / "first/rust/lib/rustlib/src/rust/library"
    sources = {}
    for manifest in root.rglob("Cargo.toml"):
        package = tomllib.loads(manifest.read_text()).get("package", {})
        name = package.get("name", "").replace("-", "_")
        if name and package.get("license"):
            sources.setdefault(name, []).append((manifest, package["license"]))
    prefix = f"first/rust/lib/rustlib/{app_lock['target']}/lib/"
    archives = []
    for path in linked_archives:
        if not path.startswith(prefix):
            continue
        name_match = re.fullmatch(r"lib([a-zA-Z0-9_]+)-[a-f0-9]+\.rlib", Path(path).name)
        if not name_match:
            raise ValueError(f"unknown Rust toolchain archive {path}")
        name = name_match.group(1)
        choices = sources.get(name, [])
        if name == "compiler_builtins":
            choices = [(file, license_name) for file, license_name in choices
                       if file.parent.name == "compiler-builtins"]
        if len(choices) != 1:
            raise ValueError(f"{name}: expected one pinned Rust source manifest, got {len(choices)}")
        source_file, license_name = choices[0]
        artifact = output / path
        if not artifact.is_file():
            raise ValueError(f"missing linked Rust toolchain archive {path}")
        archives.append({"name": name, "path": path, "sha256": sha256(artifact),
                         "license_expression": ("MIT OR Apache-2.0" if license_name == "MIT/Apache-2.0"
                                                else license_name),
                         "declared_license": license_name,
                         "source_path": source_file.relative_to(root).parent.as_posix()})
    required = {"std", "core", "alloc", "panic_unwind", "compiler_builtins"}
    if not required.issubset({archive["name"] for archive in archives}):
        raise ValueError(f"missing linked Rust runtime components: {sorted(required - {a['name'] for a in archives})}")
    license_info = app_lock["rust_standard_library"]
    notices = license_info["notices"]
    for notice in notices:
        if notice["archive"] != standard["archive"] or not HEX.fullmatch(notice["sha256"]):
            raise ValueError("invalid pinned Rust standard-library notice")
    with tarfile.open((cache or output / "sources") / standard["archive"]) as tar:
        for notice in notices:
            member = tar.getmember(notice["path"])
            if not member.isfile() or hashlib.sha256(tar.extractfile(member).read()).hexdigest() != notice["sha256"]:
                raise ValueError(f"Rust standard-library notice differs from pinned archive: {notice['path']}")
    return {"version": app_lock["rust_version"], "target": app_lock["target"],
            "license_expression": license_info["license_expression"],
            "source": {"archive": source["archive"], "sha256": source["sha256"],
                       "path": f"rust-src-{app_lock['rust_version']}/rust-src/lib/rustlib/src/rust/library"},
            "notices": notices, "archives": archives}


def input_hashes():
    inputs = [APP_LOCK, STACK_LOCK, REPOSITORY / "Cargo.lock", REPOSITORY / "Cargo.toml",
              REPOSITORY / "build.rs", REPOSITORY / "rust-toolchain.toml",
              REPOSITORY / "LICENSE", HERE / "build-app.py", HERE / "app-container-build.sh",
              HERE / "rcc-wrapper.sh",
              *sorted((REPOSITORY / "src").rglob("*")),
              *sorted((REPOSITORY / "qml").rglob("*"))]
    if any(path.is_symlink() for path in inputs):
        raise ValueError("source snapshot cannot contain symlinks")
    return {path.relative_to(REPOSITORY).as_posix(): sha256(path)
            for path in inputs if path.is_file()}


def snapshot_inputs(output, inputs):
    for relative, expected in inputs.items():
        source = REPOSITORY / relative
        if source.is_symlink() or sha256(source) != expected:
            raise ValueError(f"source changed before snapshot: {relative}")
        target = output / "source-snapshot" / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)


def run_build(media, output, cache, stack, label):
    build = output / label
    build.mkdir()
    wrapper = build / "wrappers/rcc"
    wrapper.parent.mkdir(exist_ok=True)
    epoch = stack["source_date_epoch"]
    if not isinstance(epoch, int) or epoch <= 0:
        raise ValueError("RCC reproducibility requires a positive pinned epoch")
    wrapper.write_text((HERE / "rcc-wrapper.sh").read_text().replace("@SOURCE_DATE_EPOCH@", str(epoch)))
    wrapper.chmod(0o755)
    evidence = build / "evidence"
    evidence.mkdir(exist_ok=True)
    tools = {"schema": 1, "qmake": {"invocation": "/out/prefix/bin/qmake6",
              "sha256": sha256(media / "prefix/bin/qmake6")},
             "rcc": {"discovery_path": "/out/prefix/libexec/rcc",
                     "original_invocation": "/frozen-provider/prefix/libexec/rcc",
                     "original_sha256": sha256(media / "prefix/libexec/rcc"),
                     "wrapper_sha256": sha256(wrapper), "source_date_epoch": epoch,
                     "binding": "single read-only container file bind; host provider unchanged"}}
    (evidence / "qt-build-tools.json").write_text(json.dumps(tools, indent=2, sort_keys=True) + "\n")
    command = ["podman", "run", "--rm", "--pull=always", "--platform", stack["platform"],
               "--security-opt", "label=disable", "--volume", f"{REPOSITORY.resolve()}:/workspace:ro",
               "--volume", f"{media}:/out:ro", "--volume", f"{cache}:/sources:ro",
               "--volume", f"{media}:/frozen-provider:ro",
               "--volume", f"{wrapper}:/out/prefix/libexec/rcc:ro",
               "--volume", f"{build}:/build", "--workdir", "/workspace",
               "--env", f"APT_SNAPSHOT={stack['apt_snapshot']}",
               "--env", f"CA_CERT_SHA256={stack['sources']['ca_certificates']['sha256']}",
               "--env", f"SOURCE_DATE_EPOCH={stack['source_date_epoch']}",
               "--env", f"APT_CLOSURE_SHA256={stack['apt_closure']['sha256']}",
               "--env", f"APT_INSTALLED_SHA256={stack['apt_closure']['installed_sha256']}",
               "--env", f"APT_DEBS_SHA256={stack['apt_closure']['downloaded_debs_sha256']}",
               "--env", f"FURAMI_BUILD_JOBS={os.environ.get('FURAMI_BUILD_JOBS', '2')}",
               stack["container"], "bash", "packaging/appimage/app-container-build.sh"]
    log_path = build / "build.log"
    (evidence / "container-command.json").write_text(json.dumps(command, indent=2) + "\n")
    with log_path.open("w") as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=False)
    if result.returncode:
        raise RuntimeError(f"{label} app container exited {result.returncode}; see {log_path}")
    binary = build / "bin/furami"
    if not binary.is_file() or not (build / "evidence/furami.link.map").is_file():
        raise ValueError(f"{label}: executable/link map missing")
    return binary


def build_app(media, output, cache):
    app_lock = json.loads(APP_LOCK.read_text())
    stack = json.loads(STACK_LOCK.read_text())
    if app_lock.get("schema") != 1 or app_lock.get("rust_version") != "1.97.1" or app_lock.get("target") != "x86_64-unknown-linux-gnu":
        raise ValueError("app toolchain lock must pin Rust 1.97.1 and Linux x86_64")
    if app_lock.get("media_lock_sha256") != sha256(STACK_LOCK):
        raise ValueError("app lock media SHA256 differs from frozen stack.lock.json")
    sources = app_lock.get("toolchain", {})
    if set(sources) != {"rustc", "cargo", "rust-std", "rust-src"} or any(
        not HEX.fullmatch(entry.get("sha256", "")) or
        entry.get("archive") != (f"{name}-1.97.1.tar.xz" if name == "rust-src"
                                 else f"{name}-1.97.1-x86_64-unknown-linux-gnu.tar.xz") or
        entry.get("url") != "https://static.rust-lang.org/dist/" + entry.get("archive", "")
        for name, entry in sources.items()
    ):
        raise ValueError("Rust toolchain archives require pinned upstream SHA256")
    verify_media(media, stack)
    crates = crate_sources(REPOSITORY / "Cargo.lock")
    if output.exists() and any(output.iterdir()):
        raise ValueError(f"output must be empty: {output}")
    output.mkdir(parents=True, exist_ok=True)
    cache.mkdir(parents=True, exist_ok=True)
    ca = stack["sources"]["ca_certificates"]
    all_sources = [*sources.values(), ca, *crates]
    with ThreadPoolExecutor(max_workers=8) as executor:
        list(executor.map(lambda entry: obtain(entry, cache), all_sources))
    # Keep every pinned input available to staging and recipient source assembly.
    if cache != output / "sources":
        local_sources = output / "sources"
        local_sources.mkdir(exist_ok=True)
        for entry in all_sources:
            shutil.copy2(cache / entry["archive"], local_sources / entry["archive"])
    before = input_hashes()
    media_before = sha256(media / "manifest.json")
    receipt_path = output / "build-inputs.json"
    receipt = {"schema": 1, "inputs": before, "media_path": str(media),
               "media_manifest_sha256": media_before, "source_cache": str(cache),
               "source_archives": {entry["archive"]: entry["sha256"] for entry in all_sources}}
    snapshot_inputs(output, before)
    receipt_path.write_text(json.dumps(receipt, indent=2, sort_keys=True) + "\n")
    first = run_build(media, output, cache, stack, "first")
    if before != input_hashes() or media_before != sha256(media / "manifest.json"):
        raise ValueError("source inputs changed during build")
    for relative, expected in before.items():
        check_sha(output / "source-snapshot" / relative, expected, f"source snapshot {relative}")
    for entry in all_sources:
        check_sha(cache / entry["archive"], entry["sha256"], entry["archive"])
        check_sha(output / "sources" / entry["archive"], entry["sha256"], entry["archive"])
    verify_media(media, stack)
    generated_first = generated_files(output / "first")
    if not generated_first:
        raise ValueError("CXX-Qt generated C++/headers missing from app build")
    comparison = {"status": "single-build", "sha256_first": sha256(first),
                  "generated_cpp_first": generated_first,
                  "reproducibility": "not measured; compare independent builds in CI"}
    binary = output / "bin/furami"
    binary.parent.mkdir()
    shutil.copy2(first, binary)
    evidence = output / "first/evidence"
    metadata = json.loads((evidence / "cargo-metadata.json").read_text())
    roles = dependency_roles(metadata)
    def crate_record(package):
        name, version = package["name"], package["version"]
        vendor = f"first/vendor/{name}-{version}" if package.get("source") else None
        license_file = package.get("license_file")
        if license_file and Path(license_file).is_absolute():
            try:
                license_file = str(Path(license_file).relative_to(f"/build/vendor/{name}-{version}"))
            except ValueError:
                raise ValueError(f"{name}: license file lies outside verified vendor source")
        if license_file and (".." in Path(license_file).parts or Path(license_file).is_absolute()):
            raise ValueError(f"{name}: unsafe license file path")
        return {"name": name, "version": version, "license_expression": package.get("license"),
                "license_file": license_file, "vendor_path": vendor, "source": package.get("source"),
                "checksum": next((c["sha256"] for c in crates if c["name"] == name and c["version"] == version), None),
                "role": roles[package["id"]],
                "targets": [t["kind"] for t in package["targets"]]}

    crate_inventory = [crate_record(package) for package in metadata["packages"]]
    dynamic = (evidence / "furami.dynamic.txt").read_text()
    needed = re.findall(r"\(NEEDED\).*Shared library: \[([^\]]+)\]", dynamic)
    linker_args = shlex.split((evidence / "furami-link-args.txt").read_text())
    linked_archives = [argument.replace("/build/", "first/", 1) for argument in linker_args
                       if argument.startswith("/build/") and argument.endswith((".rlib", ".a"))]
    direct_objects = retained_direct_objects(
        (evidence / "furami.link.map").read_text(), app_lock,
        json.loads((STACK_LOCK.parent / stack["apt_closure"]["file"]).read_text()))
    rust_standard_library = rust_std_closure(output, linked_archives, app_lock, output / "sources")
    manifest = {"schema": 1, "app_lock_sha256": sha256(APP_LOCK),
                "cargo_lock_sha256": sha256(REPOSITORY / "Cargo.lock"),
                "container": stack["container"], "apt_snapshot": stack["apt_snapshot"],
                "apt_source_index": app_lock["apt_source_index"],
                "apt_closure": stack["apt_closure"],
                "media": {"lock_sha256": sha256(STACK_LOCK), "manifest_sha256": media_before},
                "compilation": {"mode": "fresh",
                                "input_receipt_sha256": sha256(receipt_path),
                                "qt_tools_evidence": "first/evidence/qt-build-tools.json"},
                "inputs": before, "toolchain": sources, "source_archives": [
                    {"archive": entry["archive"], "sha256": entry["sha256"]} for entry in all_sources],
                "crates": crate_inventory,
                "binary": {"path": "bin/furami", "sha256": sha256(binary),
                           "needed": needed, "linked_archives": linked_archives,
                           "linker_arguments": linker_args,
                           "direct_objects": direct_objects,
                           "rust_standard_library": rust_standard_library,
                           "linker_args_evidence": "first/evidence/furami-link-args.txt",
                           "linker_map_evidence": "first/evidence/furami.link.map",
                           "elf": dynamic},
                "artifacts": {"bin/furami": {"type": "file", "sha256": sha256(binary)}},
                "builds": comparison,
                "evidence": {p.relative_to(output).as_posix(): sha256(p) for p in sorted(output.glob("*/evidence/*")) if p.is_file()},
                "known_gaps": ["Cargo graph names possible compiled code, not retained functions; linker map and DT_NEEDED provide actual linkage."],
                "runtime_loads": ["lib/libmpv.so"]}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(f"Built {binary}; reproducibility: {comparison['status']}; manifest: {output / 'manifest.json'}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--media-output", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-cache", type=Path)
    args = parser.parse_args()
    try:
        build_app(args.media_output.resolve(), args.output.resolve(),
                  (args.source_cache or args.output / "sources").resolve())
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
