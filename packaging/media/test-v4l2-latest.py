#!/usr/bin/env python3
"""Actual-library synthetic-kernel checks. No device node, display or audio access.

Stock baseline: python3 packaging/media/test-v4l2-latest.py --prefix /old/media/prefix --output /private/evidence/baseline --baseline
Patched suite: python3 packaging/media/test-v4l2-latest.py --prefix /new/media/prefix --output /private/evidence/patched --mpv
The output directory retains compiled probes and a JSON receipt; runtime payload/PTS
assertions live in the C consumer, not in source-text matching tests.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent


def sha256(path):
    return hashlib.file_digest(path.open("rb"), "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline", action="store_true", help="No private option; stock first-frame assertions only")
    parser.add_argument("--mpv", action="store_true", help="Also run actual libmpv null-VO integration")
    parser.add_argument("--case", help="Run only one named libavdevice scenario")
    args = parser.parse_args()
    if args.baseline and args.mpv:
        parser.error("--mpv requires the patched provider")
    prefix, output = args.prefix.resolve(strict=True), args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        parser.error("--output must be empty (preserve earlier evidence)")
    directories = [prefix / "lib", prefix / "lib/x86_64-linux-gnu", prefix / "lib64"]
    libraries = [directory for directory in directories if directory.is_dir()]
    environment = os.environ.copy()
    environment.pop("LD_PRELOAD", None)
    environment["LD_LIBRARY_PATH"] = ":".join(map(str, libraries))
    environment["PKG_CONFIG_LIBDIR"] = ":".join(str(directory / "pkgconfig") for directory in libraries)
    environment["PKG_CONFIG_PATH"] = ""
    environment["FURAMI_MEDIA_PREFIX"] = str(prefix)
    compiler = shlex.split(environment.get("CC", "cc"))
    receipt = {"kind": "actual-library/synthetic-kernel-not-hardware", "baseline": args.baseline,
               "prefix": str(prefix), "libraries": {}, "cases": []}
    for name in ("avdevice", "avformat", "avcodec", "avutil", "mpv"):
        matches = [path.resolve() for directory in libraries for path in directory.glob(f"lib{name}.so")]
        if not matches and (name != "mpv" or args.mpv):
            parser.error(f"prefix lacks lib{name}.so")
        if matches:
            receipt["libraries"][name] = {"path": str(matches[0]), "sha256": sha256(matches[0])}
    patch = HERE / "patches/ffmpeg-v4l2-latest.patch"
    if patch.is_file():
        receipt["working_patch_sha256"] = sha256(patch)
    def pkg_config(*packages):
        # FFmpeg's frozen .pc metadata uses absolute includedir/libdir, while
        # mpv uses ${prefix}; override all three for this relocated provider.
        value = subprocess.check_output(["pkg-config", f"--define-variable=prefix={prefix}",
                                         f"--define-variable=includedir={prefix / 'include'}",
                                         f"--define-variable=libdir={libraries[0]}",
                                         "--cflags", "--libs", *packages], env=environment, text=True)
        return shlex.split(value)
    def compile_probe(source, destination, packages=(), shared=False):
        command = [*compiler, "-std=c11", "-Wall", "-Wextra", "-Werror", "-O2", "-g"]
        if shared:
            command += ["-fPIC", "-shared"]
        command += [str(HERE / source), "-o", str(output / destination)]
        if packages == ("mpv",):
            # mpv/client.h is self-contained. The frozen mpv.pc lists build-only
            # Requires.private packages absent from this runtime prefix; link
            # the verified public shared library without that static closure.
            command += [f"-I{prefix / 'include'}", receipt["libraries"]["mpv"]["path"]]
        elif packages:
            command += pkg_config(*packages)
        command += [f"-Wl,-rpath-link,{directory}" for directory in libraries]
        command += ["-ldl", "-pthread"]
        subprocess.run(command, check=True, env=environment)
    compile_probe("v4l2_fixture.c", "v4l2-fixture.so", shared=True)
    compile_probe("v4l2_latest_test.c", "v4l2-latest-test", ("libavdevice", "libavformat", "libavcodec", "libavutil"))
    if args.mpv:
        compile_probe("v4l2_mpv_smoke.c", "v4l2-mpv-smoke", ("mpv",))
    formats = ("nv12", "yuyv422", "bgr24", "mjpeg")
    cases = [("off", "backlog", value) for value in formats]
    cases += [("off", "lifetime", "nv12")]
    if not args.baseline:
        cases += [("zero", "backlog", "nv12")]
        cases += [("on", "backlog", value) for value in formats]
        cases += [("on", case, "nv12") for case in (
            "single", "continuous", "two", "lifetime", "close-held", "held_duplicate", "cancel", "draincancel", "nonblock",
            "firsterror", "dqerror", "qerror", "badindex", "duplicate", "oversize", "short", "corrupt",
            "eintr", "firsteintr", "delayed", "pollerror", "pollhup", "pollnval", "pollinterrupt", "multiplanar")]
        cases += [("on", case, "nv12") for case in (
            "close-race", "close-error", "streamoff-error", "descriptor-alloc-error", "buffer-create-error",
            "req-error", "query-error", "map-error", "start-q-error", "stream-error")]
        cases += [("on", "close-held", "mjpeg")]
        cases += [("on", "unsupported", "rgb24"), ("on", "oversize", "mjpeg")]
    if args.case:
        cases = [case for case in cases if case[1] == args.case]
        if not cases:
            parser.error(f"no scenario named {args.case!r} in this mode")
    def execute(name, arguments, scenario):
        with tempfile.TemporaryDirectory(prefix="furami-v4l2-", dir="/tmp") as private:
            device = Path(private) / "synthetic-video"
            device.touch(mode=0o600)
            current = environment | {"LD_PRELOAD": str(output / "v4l2-fixture.so"),
                                     "FURAMI_V4L2_FIXTURE_PATH": str(device),
                                     "FURAMI_V4L2_SCENARIO": scenario}
            result = subprocess.run([str(output / name), *arguments], env=current,
                                    text=True, capture_output=True, timeout=15)
            log_name = "-".join((name, *arguments)) + ".log"
            (output / log_name).write_text(result.stdout + result.stderr)
            receipt["cases"].append({"command": [name, *arguments], "exit": result.returncode, "log": log_name})
            (output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
            print(result.stdout, end="", flush=True)
            if result.returncode:
                print(result.stderr, end="", flush=True)
                raise subprocess.CalledProcessError(result.returncode, [name, *arguments])
    for mode, scenario, format_name in cases:
        execute("v4l2-latest-test", [mode, scenario, format_name], scenario)
    if args.mpv:
        execute("v4l2-mpv-smoke", [], "mpv")
    receipt["verdict"] = "PASS"
    (output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"PASS {len(receipt['cases'])} actual-library subprocesses; receipt: {output / 'receipt.json'}")


if __name__ == "__main__":
    main()
