#!/usr/bin/env python3
"""Install only the SHA256-pinned official Qt 6.11.2 linux_gcc_64 archives."""

import argparse
import hashlib
import json
import sys
from pathlib import Path



def verify(path, metadata):
    sha256 = hashlib.sha256()
    sha1 = hashlib.sha1()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            sha256.update(block)
            sha1.update(block)
    if sha256.hexdigest() != metadata["sha256"] or sha1.hexdigest() != metadata["upstream_sha1"]:
        raise ValueError(f"official Qt archive checksum mismatch: {path.name}")


def relocate_metadata(prefix):
    """Apply the Linux installer relocations to Qt build metadata, not binaries."""
    vendor = b"/home/qt/work/install"
    pc_dir = prefix / "lib/pkgconfig"
    pc_files = sorted(pc_dir.glob("Qt6*.pc"))
    if not pc_files:
        raise ValueError("official Qt kit lacks Qt pkg-config metadata")
    pending = []
    for path in pc_files:
        original = path.read_bytes()
        if not original.startswith(b"prefix=" + vendor + b"\n"):
            raise ValueError(f"unexpected Qt pkg-config prefix: {path}")
        updated = b"prefix=" + str(prefix).encode() + original[len(b"prefix=" + vendor):]
        if vendor in updated:
            raise ValueError(f"unrelocated Qt pkg-config vendor path: {path}")
        pending.append((path, original, updated))

    for suffix in ("*.prl", "*.la"):
        for path in sorted((prefix / "lib").glob(suffix)):
            original = path.read_bytes()
            old_lib = vendor + b"/lib"
            replacement = (b"$$[QT_INSTALL_LIBS]" if suffix == "*.prl"
                           else str(prefix / "lib").encode())
            updated = original.replace(old_lib, replacement)
            if vendor in updated:
                raise ValueError(f"unrelocated Qt vendor path: {path}")
            if updated != original:
                pending.append((path, original, updated))

    qt_conf = prefix / "bin/qt.conf"
    if qt_conf.exists():
        raise ValueError(f"unexpected vendor qt.conf: {qt_conf}")
    pending.append((qt_conf, None, b"[Paths]\nPrefix=..\n"))

    changes = []
    for path, original, updated in pending:
        path.write_bytes(updated)
        changes.append({"path": path.relative_to(prefix).as_posix(),
                        "original_sha256": hashlib.sha256(original).hexdigest() if original is not None else None,
                        "installed_sha256": hashlib.sha256(updated).hexdigest()})
    return changes


def install(lock_path, source_dir, prefix):
    import py7zr
    qt = json.loads(lock_path.read_text())["sources"]["qt"]
    if qt["version"] != "6.11.2" or qt["kit"] != "linux_gcc_64":
        raise ValueError("wrong Qt kit")
    prefix.mkdir(parents=True, exist_ok=True)
    for metadata in qt["archives"]:
        archive = source_dir / metadata["archive"]
        verify(archive, metadata)
        destination = prefix / "lib" if metadata["component"] == "icu" else prefix
        destination.mkdir(parents=True, exist_ok=True)
        with py7zr.SevenZipFile(archive, mode="r") as package:
            package.extractall(path=destination)
        print(f"Installed official Qt {qt['version']} {metadata['component']}: {archive.name}", flush=True)

    required = ("bin/qmake", "bin/qmake6", "lib/libQt6Core.so", "lib/libQt6Qml.so",
                "lib/libQt6Quick.so", "lib/libQt6Svg.so", "lib/libQt6ShaderTools.so",
                "libexec/qmlimportscanner", "plugins/platforms/libqxcb.so",
                "qml/QtQuick/Controls/qmldir", "qml/QtQuick/Controls/impl/qmldir")
    missing = [name for name in required if not (prefix / name).exists()]
    if missing:
        raise ValueError(f"official Qt kit missing required files: {', '.join(missing)}")
    for executable in ("qmake", "qmake6"):
        data = (prefix / "bin" / executable).read_bytes()
        if any(marker in data for marker in (b"qt_prfxpath=", b"qt_epfxpath=", b"qt_hpfxpath=")):
            raise ValueError(f"unexpected Qt binary prefix marker: {executable}")
    changes = relocate_metadata(prefix)
    (prefix.parent / "qt-relocation.json").write_text(
        json.dumps({"schema": 1, "vendor_prefix": "/home/qt/work/install",
                    "kit_prefix": str(prefix), "changes": changes}, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("lock", type=Path)
    parser.add_argument("sources", type=Path)
    parser.add_argument("prefix", type=Path)
    args = parser.parse_args()
    try:
        install(args.lock, args.sources, args.prefix)
    except (OSError, KeyError, ValueError) as error:
        sys.exit(str(error))
