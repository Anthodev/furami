#!/usr/bin/env python3
"""Isolated Fedora 44 / KWin Wayland smoke for one extracted Furami AppImage.

This parent harness drives a rootless Podman container built from
``packaging/appimage/qualification-fedora44.Containerfile``. The container gets
its own private D-Bus session, a KWin Wayland compositor with XWayland, a
hardware Mesa Vulkan/GL stack and Spectacle; it never sees the host display,
session bus, audio devices, capture nodes or HOME.

What it proves (all from real screenshots and real process state, never from
copied flags):

* the candidate renders the deterministic qualification fixture in the real
  Furami window: the moving-rectangle index and the eight-cell bit code both
  advance, freeze after a confirmed pause, and advance again after resume;
* the running candidate maps its own Qt/media libraries from the extracted
  tree and uses the hardware Vulkan ICD (no lavapipe/llvmpipe);
* a missing or invalid Qt platform/xcb-graphics/SVG plugin, a missing DISPLAY
  and a missing required QML import all refuse readably before any window,
  without a crash/abort;
* `reset-confirm`, `open-close-during-opening` and `force-surface-loss` sent
  while qualification is healthy are refused without changing the owner
  generation, the pause state or the window, and a fresh snapshot plus an
  ordered drain still follow;
* the qualification close path exits 0 with an ordered teardown and no
  surviving process, and leaves a pre-existing settings sentinel byte-identical.

Observables come only from the approved finite-scalar diagnostics of the
qualification route: `qualification_state`
(phase/generation/can_toggle_pause/paused/configured/width/height/failed/quit)
and `qualification_backend` (gpu_next/x11vk/generation). The GUI-only schema-2
`session_kind` projection is not readable from logs and is not asserted here.

Fail-closed rule: a missing/failed environment (no Podman, no pinned render
node, no built image, no JSON result from the container) or an environment that
would force software rendering is reported as ``BLOCKED`` with the exact
observed reason; software rendering is never silently substituted. A product
assertion failure is reported as ``FAIL`` with the retained captures and logs.
Nothing here mutates the host desktop.

The three new files this harness uses are this script, its generated in-container
session, and ``qualification_fixture.py``. Extracted trees are mutated in place
one case at a time (renamed away, then restored) rather than copied, which keeps
each refusal case isolated without duplicating a multi-hundred-megabyte tree.
Spectacle can only capture non-interactively fullscreen, so each capture is the
whole virtual screen; the fixture placement is not assumed — the app's observed
``qt_geometry`` gives the native video host rect (the container rect, not the
letterboxed content), and the 16:9 fixture is located inside it by decode
evidence (the placement whose eight-cell bit code equals its own rectangle
index). The raw PNGs are retained for the parent's own image inspection before
any PASS is accepted.

Usage:

    python3 packaging/appimage/smoke-fedora44.py \
      --image "$OUT/release/Furami-v1.0.0-rc.2-Linux-x64.AppImage" \
      --container-image "$FEDORA_IMAGE_ID" \
      --render-device /dev/dri/renderD128 \
      --output "$EVIDENCE/fedora44-smoke"
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent

# Pinned hardware identity of the single accepted render node (plan step 4).
EXPECTED_PCI_ID = "1002:744C"
EXPECTED_PCI_SLOT = "0000:03:00.0"

# Qt XCB plugins the candidate must ship; each is mutated in one refusal case.
XCB_PLUGINS = (
    "usr/lib/qt6/plugins/platforms/libqxcb.so",
    "usr/lib/qt6/plugins/xcbglintegrations/libqxcb-egl-integration.so",
    "usr/lib/qt6/plugins/xcbglintegrations/libqxcb-glx-integration.so",
    "usr/lib/qt6/plugins/imageformats/libqsvg.so",
    "usr/lib/qt6/plugins/iconengines/libqsvgicon.so",
)
# A QML module a real launch always needs; removing it must refuse cleanly.
REQUIRED_QML_MODULE = "usr/qml/QtQuick"


class Blocker(Exception):
    """Environment/plumbing problem: report BLOCKED, never substitute a fallback."""


class Failure(Exception):
    """A real behavioural assertion failed."""


def blocked(condition: object, message: str) -> None:
    if not condition:
        raise Blocker(message)


def save(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=True, indent=2, sort_keys=True) + "\n")


def digest(path: Path) -> str:
    hasher = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def run(argv: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(argv, capture_output=True, text=True, **kwargs)


# ---------------------------------------------------------------------------
# Host-side preflight. Every failure here is BLOCKED, not a product verdict.
# ---------------------------------------------------------------------------
def preflight_image(candidate: Path, container_image: str) -> dict:
    blocked(not candidate.is_symlink(), "candidate image is a symlink")
    blocked(candidate.is_file(), f"candidate image is not a regular file: {candidate}")
    blocked(os.access(candidate, os.R_OK), f"candidate image is not readable: {candidate}")
    blocked(shutil.which("podman") is not None, "podman is not installed")
    info = run(["podman", "info", "--format", "{{.Host.Security.Rootless}} {{.Host.OCIRuntime.Name}}"])
    blocked(info.returncode == 0, f"podman info failed: {info.stderr.strip()}")
    rootless, _, runtime = info.stdout.strip().partition(" ")
    blocked(rootless == "true", "podman is not running rootless")
    inspected = run(["podman", "image", "inspect", container_image, "--format", "{{.Id}}"])
    blocked(inspected.returncode == 0,
            f"qualification container image not present locally ({container_image}): "
            f"{inspected.stderr.strip()}")
    label_inspect = run(["podman", "image", "inspect", container_image,
                         "--format", "{{json .Config.Labels}}"])
    labels = {}
    if label_inspect.returncode == 0 and label_inspect.stdout.strip():
        try:
            labels = json.loads(label_inspect.stdout.strip()) or {}
        except ValueError:
            labels = {"__unparsed__": label_inspect.stdout.strip()[:200]}
    rpms = run(["podman", "run", "--rm", "--pull=never", "--network=none", "--userns=keep-id",
                "--security-opt=label=disable", container_image,
                "cat", "/etc/furami-qualification-rpms.txt"])
    blocked(rpms.returncode == 0, f"cannot read RPM manifest from image: {rpms.stderr.strip()}")
    return {
        "candidate_sha256": digest(candidate),
        "candidate_size": candidate.stat().st_size,
        "container_image_id": inspected.stdout.strip(),
        "container_labels": labels,
        "podman_rootless": True,
        "oci_runtime": runtime or None,
        "rpm_manifest": rpms.stdout.splitlines(),
    }


def preflight_render_device(device: Path) -> dict:
    import stat as stat_module

    blocked(device.exists(), f"render device does not exist: {device}")
    blocked(stat_module.S_ISCHR(device.stat().st_mode), f"not a character device: {device}")
    node = device.name
    uevent_path = Path("/sys/class/drm") / node / "device" / "uevent"
    blocked(uevent_path.is_file(), f"no sysfs uevent for {node}: {uevent_path}")
    props = dict(line.split("=", 1) for line in uevent_path.read_text().splitlines() if "=" in line)
    pci_id, pci_slot = props.get("PCI_ID"), props.get("PCI_SLOT_NAME")
    blocked(pci_id == EXPECTED_PCI_ID and pci_slot == EXPECTED_PCI_SLOT,
            f"render node {node} is not the pinned hardware ({pci_id}/{pci_slot})")
    blocked(os.access(device, os.R_OK | os.W_OK), f"render node not accessible: {device}")
    return {"node": str(device), "pci_id": pci_id, "pci_slot": pci_slot,
            "uevent": uevent_path.as_posix(), "driver": props.get("DRIVER")}


def write_harness(harness: Path) -> None:
    harness.mkdir(parents=True, exist_ok=True)
    (harness / "session.py").write_text(CONTAINER_SESSION)
    (harness / "entry.sh").write_text(
        "#!/bin/sh\n"
        "set -eu\n"
        "# The repository WORKDIR is not relied upon; the session gets its own\n"
        "# private home and runtime dir for the compositor.\n"
        ": \"${HOME:=/tmp/furami-compositor-home}\"\n"
        ": \"${XDG_RUNTIME_DIR:=/tmp/furami-compositor-runtime}\"\n"
        "export HOME XDG_RUNTIME_DIR\n"
        "mkdir -p \"$HOME\" \"$XDG_RUNTIME_DIR\"\n"
        "chmod 700 \"$HOME\" \"$XDG_RUNTIME_DIR\"\n"
        "# Xwayland's compositor-side socket: same shape as /usr/lib/tmpfiles.d/x11.conf\n"
        "# (missing in a container because systemd-tmpfiles never runs here).\n"
        "mkdir -p /tmp/.X11-unix\n"
        "chmod 1777 /tmp/.X11-unix\n"
        "# Private D-Bus session; the compositor owns the XWayland display and\n"
        "# runs the session as its own child. AppRun's Qt/LD paths stay inside\n"
        "# the launched app and never reach kwin/spectacle.\n"
        "exec dbus-run-session -- /usr/bin/kwin_wayland \\\n"
        "    --virtual --xwayland --width 1280 --height 900 \\\n"
        "    --no-lockscreen --no-global-shortcuts \\\n"
        "    --exit-with-session \"/usr/bin/python3 /harness/session.py\"\n")
    for name in ("session.py", "entry.sh"):
        (harness / name).chmod(0o755)


def container_command(image: str, device: Path, candidate: Path, fixture: Path,
                      harness: Path, evidence: Path, name: str, cidfile: Path) -> list[str]:
    return [
        "podman", "run", "--pull=never", "--network=none", "--userns=keep-id",
        "--group-add=keep-groups", "--cap-drop=all", "--security-opt=no-new-privileges",
        "--security-opt=label=disable",
        "--name", name, "--cidfile", str(cidfile),
        "--device", str(device),
        "--mount", f"type=bind,src={candidate},dst=/input/Furami.AppImage,ro",
        "--mount", f"type=bind,src={fixture},dst=/input/fixture.y4m,ro",
        "--mount", f"type=bind,src={harness},dst=/harness,ro",
        "--mount", f"type=bind,src={evidence},dst=/evidence,rw",
        "--env", "QT_FORCE_STDERR_LOGGING=1",
        "--env", "FURAMI_SMOKE_XCB_PLUGINS=" + json.dumps(list(XCB_PLUGINS)),
        "--env", "FURAMI_SMOKE_REQUIRED_QML=" + REQUIRED_QML_MODULE,
        "--env", "HOME=/tmp/furami-compositor-home",
        "--env", "XDG_RUNTIME_DIR=/tmp/furami-compositor-runtime",
        "--workdir", "/tmp",
        image, "/harness/entry.sh",
    ]


def run_container(command: list[str], name: str, cidfile: Path, timeout: float) -> tuple[subprocess.CompletedProcess, dict]:
    """Bound the client run and record actual removal and post-removal state."""
    cleanup = {}
    try:
        completed = subprocess.run(command, capture_output=True, text=True,
                                   timeout=timeout, env=host_environment())
    except subprocess.TimeoutExpired as expired:
        def text(value):
            return value.decode("utf-8", "replace") if isinstance(value, bytes) else (value or "")
        completed = subprocess.CompletedProcess(
            command, 124, text(expired.stdout), text(expired.stderr))
        cleanup["timeout"] = "container run exceeded {:.0f}s".format(timeout)
    except OSError as error:
        completed = subprocess.CompletedProcess(command, 127, "", repr(error))
        cleanup["client_error"] = repr(error)
    finally:
        try:
            cid = cidfile.read_text().strip()
        except OSError as error:
            cid = ""
            cleanup["cidfile_error"] = repr(error)
        owned = len(cid) == 64 and all(ch in "0123456789abcdef" for ch in cid)
        target = cid if owned else name
        cleanup.update({"target": target, "ownership_recorded": owned})
        for key, argv in (
                ("before", ["podman", "container", "inspect", target]),
                ("remove", ["podman", "rm", "-f", target]),
                ("exists_after", ["podman", "container", "exists", target]),
                ("survivors", ["podman", "ps", "--all", "--no-trunc",
                               "--filter", "name=^{}$".format(name), "--format", "{{.ID}}"])):
            if key == "remove" and not owned:
                cleanup[key] = {"skipped": "no valid recorded CID; name is read-only evidence"}
                continue
            try:
                observed = run(argv, env=host_environment(), timeout=30)
                cleanup[key] = {"exit_code": observed.returncode,
                                "stdout": observed.stdout, "stderr": observed.stderr}
            except (OSError, subprocess.TimeoutExpired) as error:
                cleanup[key] = {"error": repr(error)}
        # Removal is owned explicitly; never delete by name without the recorded CID.
        cleanup["absent_after"] = (
            owned and cleanup["exists_after"].get("exit_code") == 1
            and cleanup["survivors"].get("exit_code") == 0
            and not cleanup["survivors"].get("stdout", "").strip())
    return completed, cleanup


def resolve_container_image(container_image: str) -> str:
    """Resolve a mutable tag/reference to its immutable image ID before use."""
    inspected = run(["podman", "image", "inspect", container_image, "--format", "{{.Id}}"])
    blocked(inspected.returncode == 0,
            f"cannot resolve container image {container_image}: {inspected.stderr.strip()}")
    return inspected.stdout.strip()


def host_environment() -> dict:
    """Host environment for the podman client only; no session variables leak in."""
    return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": os.environ.get("HOME", "/root"),
            "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", "/tmp")}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--image", required=True, type=Path)
    parser.add_argument("--container-image", required=True)
    parser.add_argument("--render-device", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--timeout", type=float, default=1200.0,
                        help="bounded total container wall clock")
    args = parser.parse_args(argv)

    output = args.output.resolve()
    result: dict = {"tool": "smoke-fedora44", "host": {"argv": sys.argv}}
    if output.exists() and any(output.iterdir()):
        print(json.dumps({"verdict": "BLOCKED",
                          "reason": "output directory must be new or empty: {}".format(output)},
                         indent=2), file=sys.stderr)
        return 2
    output.mkdir(parents=True, exist_ok=True)

    try:
        result["image"] = preflight_image(args.image, args.container_image)
        result["render_device"] = preflight_render_device(args.render_device)

        import importlib.util

        spec = importlib.util.spec_from_file_location("qualification_fixture",
                                                     HERE / "qualification_fixture.py")
        fixture_module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture_module)
        fixture = output / "fixture.y4m"
        result["fixture"] = fixture_module.write_fixture(fixture)

        harness = output / "harness"
        write_harness(harness)
        runtime = output / "container"
        runtime.mkdir(parents=True, exist_ok=True)

        image_id = result["image"]["container_image_id"] or resolve_container_image(args.container_image)
        nonce = "{}-{}-{}".format(output.name, os.getpid(), time.time_ns())
        name = "furami-smoke-" + hashlib.sha256(nonce.encode()).hexdigest()[:12]
        cidfile = output / "container.cid"
        command = container_command(image_id, args.render_device, args.image,
                                    fixture, harness, runtime, name, cidfile)
        started = time.monotonic()
        completed, cleanup = run_container(command, name, cidfile, args.timeout)
        result["container"] = {"command": command, "exit_code": completed.returncode,
                               "name": name, "cidfile": str(cidfile),
                               "wall_seconds": round(time.monotonic() - started, 3),
                               "cleanup": cleanup}
        (output / "container.stdout.log").write_text(completed.stdout or "")
        (output / "container.stderr.log").write_text(completed.stderr or "")

        summary_path = runtime / "summary.json"
        if not cleanup["absent_after"]:
            raise Failure("container removal/absence not verified; see container.cleanup")
        if not summary_path.is_file():
            fatal_path = runtime / "fatal.json"
            if fatal_path.is_file():
                result["container_fatal"] = fatal_path.read_text()
            raise Failure("container produced no summary.json; cause unproven "
                          f"(exit {completed.returncode}); see container logs and fatal evidence")
        summary = json.loads(summary_path.read_text())
        result["container_summary"] = summary
        if summary.get("blocked") and not summary.get("failures"):
            result["verdict"] = "BLOCKED"
            result["blocked_reason"] = summary["blocked"]
            save(output / "result.json", result)
            print(json.dumps({"verdict": "BLOCKED", "reason": summary["blocked"],
                              "output": str(output)}, indent=2), file=sys.stderr)
            return 2
        if summary.get("ok") is True and completed.returncode == 0:
            result["verdict"] = "PASS"
            save(output / "result.json", result)
            print(json.dumps({"verdict": "PASS", "output": str(output)}, indent=2))
            return 0
        result["verdict"] = "FAIL"
        result["failures"] = summary.get("failures", [])
        save(output / "result.json", result)
        print(json.dumps({"verdict": "FAIL", "failures": result["failures"],
                          "output": str(output)}, indent=2), file=sys.stderr)
        return 1
    except Blocker as error:
        result["verdict"] = "BLOCKED"
        result["blocked_reason"] = str(error)
        save(output / "result.json", result)
        print(json.dumps({"verdict": "BLOCKED", "reason": str(error)}, indent=2), file=sys.stderr)
        return 2
    except (Failure, OSError, ValueError, subprocess.SubprocessError) as error:
        result["verdict"] = "FAIL"
        result.setdefault("failures", []).append(str(error))
        save(output / "result.json", result)
        print(json.dumps({"verdict": "FAIL", "reason": str(error)}, indent=2), file=sys.stderr)
        return 1


# ---------------------------------------------------------------------------
# In-container session harness (runs inside the private D-Bus/KWin session).
# ---------------------------------------------------------------------------
CONTAINER_SESSION = r'''#!/usr/bin/env python3
"""In-container qualification session driven inside a private KWin Wayland session."""
import hashlib, json, os, re, select, shutil, stat, subprocess, sys, time
from pathlib import Path

EVIDENCE = Path("/evidence")
XCB_PLUGINS = tuple(json.loads(os.environ["FURAMI_SMOKE_XCB_PLUGINS"]))
REQUIRED_QML = os.environ["FURAMI_SMOKE_REQUIRED_QML"]
# Mandatory qualification teardown milestones, in the order they must appear.
MANDATORY_TEARDOWN = ("owner_destroy_complete", "ui_stop_ack", "native_destroy_begin",
                      "native_destroy", "qt_quit_authorized", "qt_event_loop_exit code=0")
# The recorded Radeon ICD pins the isolated app so a software device cannot answer.
RADEON_ICD = None
FAILURES = []
BLOCKERS = []
NOTES = {}
APP_SESSIONS = []


def fail(message):
    FAILURES.append(message)


def block(message):
    """Environment/GPU/tooling obstacle: BLOCKED, never a software fallback."""
    BLOCKERS.append(message)


def store(name, value):
    path = EVIDENCE / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=True, indent=2, sort_keys=True) + "\n")


def store_text(name, text):
    path = EVIDENCE / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def sha256(path):
    hasher = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def private_environment(home):
    env = dict(os.environ)
    for key in ("LD_LIBRARY_PATH", "LD_PRELOAD", "QT_PLUGIN_PATH", "QT_QPA_PLATFORM",
                "QT_QPA_PLATFORM_PLUGIN_PATH", "QML_IMPORT_PATH", "QML2_IMPORT_PATH",
                "QT_QUICK_CONTROLS_STYLE", "FURAMI_MEDIA_PREFIX", "VK_ICD_FILENAMES",
                "VK_DRIVER_FILES", "GALLIUM_DRIVER", "LIBGL_ALWAYS_SOFTWARE",
                "MESA_LOADER_DRIVER_OVERRIDE"):
        env.pop(key, None)
    env.update({
        "HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_CACHE_HOME": str(home / ".cache"), "XDG_STATE_HOME": str(home / ".state"),
        "XDG_DATA_HOME": str(home / ".local/share"),
        "XDG_RUNTIME_DIR": str(home / "run"),
    })
    if RADEON_ICD:
        # Explicit isolated-environment restriction: only the recorded Radeon ICD
        # is offered to the launched candidate, so a software device can never
        # answer. This is a test-environment pin, not a product/provider change.
        env["VK_DRIVER_FILES"] = RADEON_ICD
        env.pop("VK_ICD_FILENAMES", None)
    return env


def make_private_root(root):
    """Create an isolated 0700 HOME/XDG tree."""
    for path in (root, root / ".config", root / ".config/furami", root / ".cache",
                 root / ".state", root / ".local/share", root / "run"):
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        path.chmod(0o700)
    return root


def prepare_private_roots():
    home = Path("/tmp/furami-smoke/home")
    if home.exists():
        shutil.rmtree(home)
    make_private_root(home)
    sentinel = home / ".config/furami/settings.json"
    sentinel.write_bytes(b"{\"schema\": 999, \"malformed\": true, \"note\": \"smoke sentinel\"\n")
    sentinel.chmod(0o600)
    return home, sentinel


def sentinel_state(sentinel):
    info = sentinel.stat()
    directory = sentinel.parent.stat()
    return {
        "sha256": sha256(sentinel), "size": info.st_size,
        "mode": oct(stat.S_IMODE(info.st_mode)),
        "st_dev": info.st_dev, "st_ino": info.st_ino,
        "mtime_ns": info.st_mtime_ns, "ctime_ns": info.st_ctime_ns,
        "config_dir": sorted(p.name for p in sentinel.parent.iterdir()),
        "config_dir_mode": oct(stat.S_IMODE(directory.st_mode)),
        "config_dir_mtime_ns": directory.st_mtime_ns,
    }


def tree_fingerprint(root):
    """Content fingerprint of an extracted tree (relative path, mode, size, hash/link)."""
    hasher = hashlib.sha256()
    entries = 0
    total_bytes = 0
    for path in sorted(Path(root).rglob("*")):
        try:
            info = path.lstat()
        except OSError:
            continue
        relative = path.relative_to(root).as_posix()
        mode = oct(stat.S_IMODE(info.st_mode))
        if stat.S_ISLNK(info.st_mode):
            target = os.readlink(str(path))
            hasher.update("L|{}|{}|{}\n".format(relative, mode, target).encode())
            entries += 1
            continue
        if stat.S_ISDIR(info.st_mode):
            hasher.update("D|{}|{}\n".format(relative, mode).encode())
            entries += 1
            continue
        if not stat.S_ISREG(info.st_mode):
            hasher.update("O|{}|{}\n".format(relative, mode).encode())
            entries += 1
            continue
        hasher.update("F|{}|{}|{}|{}\n".format(relative, mode, info.st_size,
                                              sha256(path)).encode())
        entries += 1
        total_bytes += info.st_size
    return {"entries": entries, "bytes": total_bytes, "digest": hasher.hexdigest()}


def target_fingerprint(target):
    """Content fingerprint of one mutated path so its restoration is provable."""
    info = target.lstat()
    if stat.S_ISDIR(info.st_mode):
        return tree_fingerprint(target)
    if stat.S_ISLNK(info.st_mode):
        return {"kind": "link", "target": os.readlink(str(target)),
                "mode": oct(stat.S_IMODE(info.st_mode))}
    return {"kind": "file", "size": info.st_size, "sha256": sha256(target),
            "mode": oct(stat.S_IMODE(info.st_mode))}


def proc_identity(pid):
    """(ppid, pgrp, session) for a pid, or None."""
    try:
        stat_line = Path("/proc/{}/stat".format(pid)).read_text()
    except OSError:
        return None
    tail = stat_line.rsplit(")", 1)[-1].split()
    if len(tail) < 4:
        return None
    return int(tail[1]), int(tail[2]), int(tail[3])


def process_ancestry(pid):
    chain = []
    current = pid
    for _ in range(64):
        if current <= 1:
            break
        chain.append(current)
        identity = proc_identity(current)
        if identity is None:
            break
        current = identity[0]
    return chain


def survivors_of(root, app_pid, app_session):
    """Processes still holding the candidate tree, by path, ancestry or session."""
    root_text = str(root)
    own = process_ancestry(os.getpid())
    survivors = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        if pid == os.getpid() or pid in own or pid == app_pid:
            continue
        try:
            cmdline = (entry / "cmdline").read_bytes().replace(b"\0", b" ").decode(
                "utf-8", "replace").strip()
        except OSError:
            cmdline = ""
        links = {}
        for name in ("exe", "cwd", "root"):
            try:
                links[name] = os.readlink(str(entry / name))
            except OSError:
                links[name] = ""
        identity = proc_identity(pid)
        descendant = app_pid in process_ancestry(pid)
        same_session = app_session is not None and identity is not None and identity[2] == app_session
        if (root_text in cmdline or root_text in links["exe"] or root_text in links["cwd"]
                or descendant or same_session):
            survivors.append({"pid": pid, "cmdline": cmdline, "links": links,
                              "descendant": descendant, "same_session": same_session})
    return survivors


def teardown_chain(app):
    """Validate the mandatory teardown milestones as structured events in order."""
    chain = {}
    for code in ("owner_destroy_complete", "ui_stop_ack", "native_destroy_begin",
                 "native_destroy", "qt_quit_authorized", "qt_event_loop_exit"):
        values = event_records(app, code)
        chain[code] = values[-1][1] if values else None
    indices = []
    for marker in MANDATORY_TEARDOWN:
        code = marker.split(" ")[0]
        values = event_records(app, code)
        indices.append(values[-1][0] if values else None)
    ordered = all(i is not None for i in indices) and indices == sorted(indices)
    required = {
        "generation": ("owner_destroy_complete", "ui_stop_ack", "native_destroy_begin",
                       "native_destroy", "qt_quit_authorized"),
        "root": ("ui_stop_ack", "native_destroy_begin", "native_destroy", "qt_quit_authorized"),
        "host": ("ui_stop_ack", "native_destroy_begin", "native_destroy"),
    }
    expected = app.get("teardown_identity", {})
    identities = {field: {code: (chain[code] or {}).get(field) for code in codes}
                  for field, codes in required.items()}
    correlated = all(
        type(expected.get(field)) is int and expected[field] > 0
        and all(type(value) is int and value == expected[field]
                for value in values.values())
        for field, values in identities.items())
    field_ok = (chain["native_destroy_begin"] or {}).get("parent_preserved") is True \
        and (chain["native_destroy"] or {}).get("authorized") is True \
        and (chain["qt_event_loop_exit"] or {}).get("exit_code") == 0 \
        and (chain["qt_event_loop_exit"] or {}).get("owner_cleanup_authorized") is True
    return {"events": chain, "present": [m for m, i in zip(MANDATORY_TEARDOWN, indices)
                                         if i is not None],
            "ordered": ordered, "correlated": correlated, "fields_ok": field_ok,
            "identities": identities, "expected_identity": expected,
            "complete": ordered and correlated and field_ok}


def extract_candidate(home):
    work = home / "with space"
    work.mkdir(parents=True, exist_ok=True)
    image = work / "Furami.AppImage"
    shutil.copy2("/input/Furami.AppImage", image)
    image.chmod(0o755)
    completed = subprocess.run([str(image), "--appimage-extract"], cwd=str(work),
                               capture_output=True, text=True)
    root = work / "squashfs-root"
    if completed.returncode != 0 or not (root / "AppRun").is_file():
        raise SystemExit("FATAL: candidate extraction failed: " + completed.stderr[-2000:])
    return root


def spawn_app(root, env, extra_argv=()):
    argv = [str(root / "AppRun"), "--qualification-video", "/input/fixture.y4m",
            "--qualification-stdin"] + list(extra_argv)
    proc = subprocess.Popen(argv, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, bufsize=0, start_new_session=True)
    os.set_blocking(proc.stdin.fileno(), False)
    identity = proc_identity(proc.pid)
    APP_SESSIONS.append({"pid": proc.pid, "identity": identity, "root": str(root)})
    return {"proc": proc, "argv": argv, "identity": identity,
            "rows": [], "eof": False, "pending": b""}


def finish_app(app, root, label):
    """Always reap the child and retain observed survivors before removing them."""
    proc = app["proc"]
    result = {"pid": proc.pid, "identity": app.get("identity")}
    try:
        if proc.poll() is None:
            fail("{}: child still running at scenario cleanup".format(label))
            proc.kill()
        result["exit_code"] = proc.wait(timeout=10)
    except (OSError, subprocess.TimeoutExpired) as error:
        result["reap_error"] = repr(error)
        fail("{}: child reap failed: {!r}".format(label, error))
    try:
        pump(app, 0.5)
    except OSError as error:
        result["read_error"] = repr(error)
        fail("{}: final output read failed: {!r}".format(label, error))
    identity = app.get("identity")
    survivors = survivors_of(root, proc.pid, identity[2] if identity else proc.pid)
    result["survivors"] = survivors
    if survivors:
        fail("{}: surviving candidate processes: {}".format(label, survivors))
    result["survivor_cleanup"] = []
    for survivor in survivors:
        try:
            os.kill(survivor["pid"], 9)
            result["survivor_cleanup"].append({"pid": survivor["pid"], "signal": 9})
        except OSError as error:
            result["survivor_cleanup"].append({"pid": survivor["pid"], "error": repr(error)})
    if survivors:
        time.sleep(0.1)
    result["survivors_after_cleanup"] = survivors_of(
        root, proc.pid, identity[2] if identity else proc.pid)
    if result["survivors_after_cleanup"]:
        fail("{}: candidate survivors remain after cleanup: {}".format(
            label, result["survivors_after_cleanup"]))
    return result


def pump(app, seconds):
    """Read available stdout for a bounded period, retaining partial lines.

    A line is appended only once its newline arrives, so a JSON record or marker
    split across two reads is never dropped or truncated.
    """
    deadline = time.monotonic() + seconds
    fd = app["proc"].stdout.fileno()
    buffer = app.setdefault("pending", b"")
    while time.monotonic() < deadline:
        readable, _, _ = select.select([fd], [], [],
                                      min(0.1, max(0.0, deadline - time.monotonic())))
        if not readable:
            if app["proc"].poll() is not None:
                break
            continue
        chunk = os.read(fd, 65536)
        if not chunk:
            app["eof"] = True
            break
        buffer += chunk
        while b"\n" in buffer:
            line, buffer = buffer.split(b"\n", 1)
            app["rows"].append(line.decode("utf-8", "replace"))
    app["pending"] = buffer
    if app["eof"] and app["pending"]:
        app["rows"].append(app["pending"].decode("utf-8", "replace"))
        app["pending"] = b""


def send(app, line):
    view = memoryview((line + "\n").encode())
    deadline = time.monotonic() + 30
    while view:
        if app["proc"].poll() is not None:
            fail("app exited while sending " + repr(line))
            return False
        writable = select.select([], [app["proc"].stdin.fileno()], [], 0.2)[1]
        if not writable:
            if time.monotonic() > deadline:
                fail("stdin write deadline exceeded for " + repr(line))
                return False
            continue
        try:
            written = os.write(app["proc"].stdin.fileno(), view[:4096])
        except BlockingIOError:
            continue
        view = view[written:]
    pump(app, 0.05)
    return True


def json_records(app, offset=0):
    """(original row index, parsed record) for each parsable JSON row."""
    values = []
    for index in range(offset, len(app["rows"])):
        row = app["rows"][index].strip()
        if not row.startswith("{"):
            continue
        try:
            values.append((index, json.loads(row)))
        except ValueError:
            continue
    return values


def state_fields(record):
    fields = record.get("fields")
    return fields if isinstance(fields, dict) else record


def qualification_state(app, offset=0):
    """Last observable `qualification_state` event at/after `offset`."""
    for _, record in reversed(json_records(app, offset)):
        if record.get("event_code") == "qualification_state":
            return state_fields(record)
    return None


def refresh_state(app, timeout=30.0):
    """Request a fresh snapshot (which forces one state emission) and read it."""
    offset = len(app["rows"])
    if not send(app, "snapshot"):
        return None
    deadline = time.monotonic() + timeout
    state = None
    while time.monotonic() < deadline:
        pump(app, 0.2)
        state = qualification_state(app, offset)
        if state is not None:
            return state
        if app["proc"].poll() is not None:
            break
    fail("snapshot: no qualification_state within {}s".format(timeout))
    return None


def wait_state(app, predicate, label, timeout=90.0):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        state = refresh_state(app, timeout=max(1.0, min(30.0, deadline - time.monotonic())))
        if state is None:
            return None
        last = state
        if predicate(state):
            return state
        if app["proc"].poll() is not None:
            break
        time.sleep(0.1)
    fail("{}: qualification_state never satisfied the condition within {}s (last={})".format(
        label, timeout, json.dumps(last)[:400] if last else None))
    return None


def backend_states(app):
    """Parsed `qualification_backend` records (gpu_next/x11vk booleans)."""
    return [fields for _, fields in event_records(app, "qualification_backend")]


def healthy_state(state):
    """Ready for qualification: decoded fixture observed at the exact size."""
    return (str(state.get("phase")) == "Ready" and state.get("configured") is True
            and state.get("width") == 320 and state.get("height") == 180
            and state.get("paused") is False and state.get("failed") is False
            and state.get("can_toggle_pause") is True)


def event_records(app, code, offset=0):
    """(original row index, fields) for every record with `event_code == code`."""
    values = []
    for index, record in json_records(app, offset):
        if record.get("event_code") == code:
            values.append((index, state_fields(record)))
    return values


def last_event(app, code):
    values = event_records(app, code)
    return values[-1][1] if values else None


def wait_for_event(app, code, predicate, label, timeout=60.0):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        pump(app, 0.2)
        values = event_records(app, code)
        if values:
            last = values[-1][1]
            if predicate(last):
                return last
        if app["proc"].poll() is not None:
            break
    fail("{}: no {} event satisfied the condition within {}s (last={})".format(
        label, code, timeout, last))
    return None


def wait_geometry(app, predicate, label, timeout=60.0):
    return wait_for_event(app, "qt_geometry", predicate, label, timeout)


def proc_maps(pid):
    path = Path("/proc/{}/maps".format(pid))
    try:
        text = path.read_text()
        store_text("application-main.maps", text)
        return text
    except OSError as error:
        store("application-main-maps-error.json", {"path": str(path), "error": repr(error)})
        fail("candidate maps unreadable: {!r}".format(error))
        return ""


# Library families that must come from the extracted candidate; anything from
# the host filesystem in these families means a mixed system fallback.
CANDIDATE_ONLY_FAMILIES = ("libQt6", "libQt5", "libmpv", "libplacebo", "libass",
                           "libavcodec", "libavformat", "libavutil", "libavfilter",
                           "libswscale", "libswresample", "libavdevice", "libx264", "libx265",
                           "libvpx", "libdav1d", "libSvtAv1", "libaom", "libmp3lame",
                           "libvorbis", "libopus", "libFLAC")


def analyse_maps(maps, extract_root):
    prefix = str(extract_root) + "/"
    paths = []
    for line in maps.splitlines():
        # Pathname is everything after the fifth field, so an extraction path
        # containing spaces is never truncated.
        parts = line.split(None, 5)
        if len(parts) >= 6:
            paths.append(parts[5].strip())

    def family(path):
        name = path.rsplit("/", 1)[-1]
        return [f for f in CANDIDATE_ONLY_FAMILIES if f in name]

    candidate_only = {p for p in paths if family(p)}
    return {
        "qt_from_candidate": sorted({p for p in candidate_only if p.startswith(prefix)
                                     and "libQt" in p}),
        "media_from_candidate": sorted({p for p in candidate_only if p.startswith(prefix)
                                        and "libQt" not in p}),
        # External Qt/media/provider mappings: a mixed system fallback.
        "external_candidate_families": sorted({p for p in candidate_only
                                               if not p.startswith(prefix)}),
        "hardware_icd_mapped": sorted({p for p in paths if "libvulkan_radeon" in p}),
        "software_icd_mapped": sorted({p for p in paths
                                       if "libvulkan_lvp" in p or "llvmpipe" in p
                                       or "lavapipe" in p}),
    }


def radeon_icd(icd_dirs):
    for directory in sorted(icd_dirs):
        matches = sorted(Path(directory).glob("*radeon*.json"))
        if matches:
            return str(matches[0])
    return None


def run_vulkan_facts():
    facts = {"icd_dirs": {}, "software_icd_libs": [], "providers_found": {}, "radeon_icd": None}
    for directory in ("/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"):
        path = Path(directory)
        if path.is_dir():
            facts["icd_dirs"][directory] = sorted(p.name for p in path.iterdir())
    for root in ("/usr/lib64", "/usr/lib"):
        for pattern in ("libvulkan_lvp.so*", "libvulkan_lavapipe*"):
            facts["software_icd_libs"].extend(str(p) for p in Path(root).glob(pattern))
    for program in ("mpv", "ffmpeg", "ffprobe"):
        facts["providers_found"][program] = shutil.which(program)
    facts["radeon_icd"] = radeon_icd(facts["icd_dirs"])
    unrestricted = subprocess.run(["vulkaninfo", "--summary"], capture_output=True, text=True)
    facts["vulkaninfo_exit"] = unrestricted.returncode
    facts["vulkaninfo_summary"] = unrestricted.stdout[-6000:]
    facts["vulkaninfo_stderr"] = unrestricted.stderr[-1000:]
    facts["enumerated_device_names"] = re.findall(r"deviceName\s*=\s*(.+)", unrestricted.stdout)
    if facts["radeon_icd"]:
        restricted_env = dict(os.environ, VK_DRIVER_FILES=facts["radeon_icd"])
        restricted_env.pop("VK_ICD_FILENAMES", None)
        restricted = subprocess.run(["vulkaninfo", "--summary"], capture_output=True, text=True,
                                    env=restricted_env)
        facts["restricted_vulkaninfo_exit"] = restricted.returncode
        facts["restricted_vulkaninfo_summary"] = restricted.stdout[-4000:]
        facts["restricted_device_names"] = re.findall(r"deviceName\s*=\s*(.+)", restricted.stdout)
    return facts


# --- Screenshot decoding (Pillow inside the container) -----------------------
def host_rect(geometry, image_size):
    """Physical-pixel rect of the native video host from observed geometry.

    ``qt_geometry`` reports the videoContainer rect in logical pixels; the
    capture is in physical pixels, so it is scaled by the observed dpr. The host
    itself is NOT assumed 16:9: Main.qml anchors the container to the whole
    window, and mpv letterboxes the 16:9 fixture inside it.
    """
    try:
        dpr = float(geometry.get("dpr") or 1.0)
        x = float(geometry["video_x"]) * dpr
        y = float(geometry["video_y"]) * dpr
        width = float(geometry["video_width"]) * dpr
        height = float(geometry["video_height"]) * dpr
    except (KeyError, TypeError, ValueError):
        return None
    if width <= 0 or height <= 0 or dpr <= 0:
        return None
    image_width, image_height = image_size
    x0, y0 = max(0, int(x)), max(0, int(y))
    x1, y1 = min(image_width, int(x + width + 0.5)), min(image_height, int(y + height + 0.5))
    if x1 - x0 < 160 or y1 - y0 < 90:
        return None
    return (x0, y0, x1, y1)


def fixture_fit(host, vertical):
    """Largest centered 16:9 rectangle inside ``host`` at the given vertical align."""
    x0, y0, x1, y1 = host
    host_width, host_height = x1 - x0, y1 - y0
    if host_width / host_height > 16 / 9:
        height = host_height
        width = int(height * 16 / 9)
        left = x0 + (host_width - width) // 2
        top = y0
    else:
        width = host_width
        height = int(width * 9 / 16)
        left = x0
        top = {"center": y0 + (host_height - height) // 2, "top": y0,
               "bottom": y1 - height}[vertical]
    if width < 160 or height < 90:
        return None
    return (left, top, left + width, top + height)


def candidate_boxes(geometry, image_size):
    """Candidate placements for the letterboxed fixture inside the host rect."""
    host = host_rect(geometry, image_size)
    if host is None:
        return []
    boxes = []
    for vertical in ("center", "top", "bottom"):
        box = fixture_fit(host, vertical)
        if box is not None and box not in boxes:
            boxes.append(box)
    return boxes


def locate_box(png_path, boxes):
    """Pick the placement whose decode is internally consistent (bits == index).

    The fixture is located by evidence, never assumed: the chosen placement must
    decode to a frame whose eight-cell bit code equals the moving-rectangle
    index and falls inside the fixture's 0..119 range.
    """
    for box in boxes:
        sig = signature(png_path, box)
        if (sig["index"] is not None and sig["bits_mismatch"] is False
                and 0 <= sig["index"] <= 119):
            return box, sig
    return None, None


def signature(png_path, box):
    from PIL import Image

    image = Image.open(png_path).convert("L")
    size = list(image.size)
    if box is None:
        return {"size": size, "region": None, "bits": None, "rectangle_center": None,
                "index": None, "bits_mismatch": None}
    region = image.crop(box)
    rw, rh = region.size
    pixels = region.load()

    def sample(fx, fy, radius=6):
        cx, cy = int(fx * rw), int(fy * rh)
        total = count = 0
        for dy in range(-radius, radius + 1):
            for dx in range(-radius, radius + 1):
                px, py = cx + dx, cy + dy
                if 0 <= px < rw and 0 <= py < rh:
                    total += pixels[px, py]
                    count += 1
        return total / max(count, 1)

    bits = 0
    for index in range(8):
        if sample((80 + 20 * index + 8) / 320, (120 + 8) / 180) > 126:
            bits |= 1 << index

    band_top, band_bottom = int(60 / 180 * rh), int(100 / 180 * rh)
    xs = [x for y in range(band_top, band_bottom) for x in range(rw) if pixels[x, y] > 150]
    rectangle_center = (min(xs) + max(xs)) / 2 if xs else None
    index = None
    if rectangle_center is not None:
        index = round(((rectangle_center / rw) * 320 - 36) / 2)
    return {"size": size, "region": list(box), "bits": bits,
            "rectangle_center": rectangle_center, "index": index,
            "bits_mismatch": None if index is None else (bits != (index % 256))}


def grade(label, facts):
    """Return a decodable fixture signature or record a failure and return None."""
    if facts is None:
        return None
    sig = facts["signature"]
    if sig["region"] is None or sig["index"] is None or sig["bits_mismatch"] is not False:
        fail("capture {} has no decodable fixture pattern: {}".format(label, sig))
        return None
    if not 0 <= sig["index"] <= 119:
        fail("capture {} index {} outside the fixture's 0..119 frames".format(label, sig["index"]))
        return None
    return sig


def pixel_mean_abs_diff(first, second, box):
    from PIL import Image, ImageChops

    a = Image.open(first).convert("L").crop(box)
    b = Image.open(second).convert("L").crop(box)
    if a.size != b.size:
        return None
    histogram = ImageChops.difference(a, b).histogram()
    total = sum(histogram)
    return sum(value * count for value, count in enumerate(histogram)) / max(total, 1)


def spectacle(path):
    attempts = []
    for argv in (["spectacle", "--background", "--nonotify", "--fullscreen", "--output", str(path)],
                 ["spectacle", "-b", "-n", "-f", "-o", str(path)]):
        completed = subprocess.run(argv, capture_output=True, text=True)
        attempts.append({"argv": argv, "exit_code": completed.returncode,
                         "stdout": completed.stdout, "stderr": completed.stderr})
        store(path.stem + "-spectacle.json", attempts)
        if completed.returncode == 0 and path.is_file() and path.stat().st_size > 0:
            return {"argv": argv, "exit": completed.returncode,
                    "size": path.stat().st_size, "sha256": sha256(path)}
    return None


# --- Scenarios --------------------------------------------------------------
def scenario_main(home, root):
    app = spawn_app(root, private_environment(home))
    notes = {"argv": app["argv"], "pid": app["proc"].pid}
    captured = []
    try:
        scenario_main_run(app, root, notes, captured)
    except Exception as error:
        notes["error"] = repr(error)
        raise
    finally:
        notes["cleanup"] = finish_app(app, root, "main")
        store_text("application-main.log", "\n".join(app["rows"]))
        store("main.json", {"notes": notes, "captures": captured})


def scenario_main_run(app, root, notes, captured):
    # The real owner is ready only once the fixture is decoded and the phase left
    # the opening states; wait for that exact observable state.
    ready = wait_state(app, healthy_state, "healthy ready state", timeout=150)
    if ready is None:
        # Hardware Vulkan was already proven before launch (pinned ICD + restricted
        # vulkaninfo), so a healthy-Ready failure is a product failure with exact
        # evidence retained, never reclassified as an environment problem by
        # matching GPU/Vulkan/EGL keywords in the log.
        observed = qualification_state(app)
        notes["ready_failure_observed_state"] = observed
        notes["ready_failure_tail"] = app["rows"][-60:]
        notes["ready_failure_exit"] = app["proc"].poll()
        fail("qualification never reached a healthy Ready state within 150s; last state={} "
             "exit={}".format(json.dumps(observed)[:400] if observed else None,
                              app["proc"].poll()))
        return

    notes.update({"ready_state": ready,
                  "decoded_size": [ready.get("width"), ready.get("height")]})
    backends = backend_states(app)
    notes["backend_events"] = backends
    if not backends:
        fail("no qualification_backend event: video output / GPU context unproven")
    else:
        last_backend = backends[-1]
        notes["backend_last"] = last_backend
        if last_backend.get("gpu_next") is not True or last_backend.get("x11vk") is not True:
            fail("qualification_backend did not observe gpu-next with an x11vk context: {}".format(
                last_backend))

    maps = analyse_maps(proc_maps(app["proc"].pid), root)
    notes["maps"] = maps
    if not maps["qt_from_candidate"] or not maps["media_from_candidate"]:
        fail("running candidate does not map its own Qt/media libraries from the extracted tree: "
             "{}".format(maps))
    if maps["external_candidate_families"]:
        fail("running candidate maps Qt/media/provider libraries from outside the extracted tree "
             "(mixed system fallback): {}".format(maps["external_candidate_families"]))
    if maps["software_icd_mapped"]:
        fail("running candidate mapped a software Vulkan ICD even though only the recorded Radeon "
             "ICD was offered: {}".format(maps["software_icd_mapped"]))
    if not maps["hardware_icd_mapped"]:
        fail("candidate hardware Vulkan ICD mapping unproven; no cause inferred: {}".format(maps))

    # Real fullscreen evidence from native readback, plus the observed geometry
    # of the video host (never assumed).
    send(app, "fullscreen")
    readback = wait_for_event(app, "fullscreen_preference_readback",
                              lambda f: f.get("effective") is True,
                              "fullscreen native readback", timeout=30)
    native_state = last_event(app, "fullscreen_native_state_observed")
    geometry = wait_geometry(app, lambda g: g.get("root_width") == 1280
                             and g.get("root_height") == 900, "fullscreen geometry", timeout=60)
    notes["fullscreen_readback"] = readback
    notes["fullscreen_native_state"] = native_state
    notes["geometry"] = geometry
    app["teardown_identity"] = {
        "generation": ready.get("generation"),
        "root": (geometry or {}).get("root"),
        "host": (last_event(app, "native_publish") or {}).get("host"),
    }
    if geometry is None:
        fail("fullscreen root geometry 1280x900 was never observed")
    if readback is None:
        fail("fullscreen native readback never reported effective fullscreen")


    def capture(label, box):
        path = EVIDENCE / (label + ".png")
        facts = spectacle(path)
        if facts is None:
            fail("no screenshot obtained for " + label + "; cause unproven, see Spectacle receipt")
            return None
        facts["label"] = label
        facts["signature"] = signature(path, box)
        captured.append(facts)
        return facts

    probe = capture("moving-a", None)
    box = None
    if geometry is not None and probe is not None:
        boxes = candidate_boxes(geometry, tuple(probe["signature"]["size"]))
        notes["candidate_boxes"] = boxes
        box, probe_signature = locate_box(EVIDENCE / "moving-a.png", boxes)
        notes["chosen_box"] = list(box) if box else None
        if box is None:
            fail("no placement inside the observed host rect decodes to a consistent fixture "
                 "frame; candidates were {}".format(boxes))
        else:
            probe["signature"] = probe_signature

    time.sleep(0.6)
    second = capture("moving-b", box)
    a = grade("moving-a", probe) if box else None
    b = grade("moving-b", second) if box else None
    if a is not None and b is not None and a["index"] == b["index"]:
        fail("qualification video did not advance between captures: {}".format(a["index"]))

    generation = ready.get("generation")
    frozen_index = None
    if not isinstance(generation, int):
        fail("qualification state exposes no generation for the pause command")
    else:
        if not send(app, "pause {}".format(generation)):
            fail("pause command could not be submitted")
        paused = wait_state(app, lambda s: s.get("paused") is True
                            and s.get("can_toggle_pause") is True
                            and s.get("generation") == generation, "confirmed pause", timeout=30)
        if paused is not None:
            notes["paused_state"] = paused
            freeze_a = capture("paused-a", box)
            time.sleep(1.5)
            freeze_b = capture("paused-b", box)
            if box:
                sa, sb = grade("paused-a", freeze_a), grade("paused-b", freeze_b)
                if sa is not None and sb is not None:
                    frozen_index = sb["index"]
                    if sa["index"] != sb["index"] or sa["bits"] != sb["bits"]:
                        fail("paused video kept changing: {} vs {}".format(sa, sb))
                    mean = pixel_mean_abs_diff(EVIDENCE / "paused-a.png",
                                               EVIDENCE / "paused-b.png", box)
                    freeze_a["pause_pixel_mean_abs_diff"] = mean
                    if mean is None or mean > 2.0:
                        fail("paused captures differ beyond dither tolerance: mean={}".format(mean))
            if not send(app, "pause {}".format(generation)):
                fail("resume command could not be submitted")
            resumed = wait_state(app, lambda s: s.get("paused") is False
                                 and s.get("can_toggle_pause") is True
                                 and s.get("generation") == generation, "confirmed resume",
                                 timeout=30)
            notes["resumed_state"] = resumed
            healthy_again = wait_state(app, healthy_state, "healthy state after resume", timeout=30)
            notes["healthy_after_resume"] = healthy_again is not None
            time.sleep(0.8)
            after = capture("resumed", box)
            if box:
                graded_after = grade("resumed", after)
                if graded_after is not None and frozen_index is not None \
                        and graded_after["index"] == frozen_index:
                    fail("qualification video did not advance after resume")

    # Forbidden native shortcuts must be refused through the Rust parser: each
    # needs its own fresh rejection, and generation/pause/phase stay unchanged.
    baseline = refresh_state(app) or ready
    refusals = {}
    window_events_before = sum(1 for row in app["rows"] if "native_root" in row)
    for command in ("reset-confirm", "open-close-during-opening", "force-surface-loss"):
        offset = len(app["rows"])
        send(app, command)
        rejection = None
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            pump(app, 0.2)
            values = event_records(app, "qualification_command_rejected", offset)
            if values:
                rejection = values[-1][1]
                break
            if app["proc"].poll() is not None:
                break
        current = refresh_state(app) or {}
        entry = {
            "alive": app["proc"].poll() is None,
            "rejection": bool(rejection) and rejection.get("kind") == "CommandRejected"
                         and rejection.get("accepted") is False,
            "phase": current.get("phase"), "generation": current.get("generation"),
            "paused": current.get("paused"), "can_toggle_pause": current.get("can_toggle_pause"),
            "failed": current.get("failed"),
            "generation_unchanged": current.get("generation") == baseline.get("generation"),
            "paused_unchanged": current.get("paused") == baseline.get("paused"),
            "window_events": sum(1 for row in app["rows"] if "native_root" in row),
        }
        refusals[command] = entry
        if not entry["alive"]:
            fail("app exited after {!r}; a refused shortcut must not act".format(command))
            break
        if not entry["rejection"]:
            fail("{!r} produced no fresh accepted=false CommandRejected observation".format(
                command))
        if str(current.get("phase")) not in ("Ready",):
            fail("{!r} left the qualification phase unhealthy: {}".format(
                command, current.get("phase")))
        if entry["failed"] is not False or entry["can_toggle_pause"] is not True:
            fail("{!r} changed failed/can_toggle_pause: {}".format(command, current))
        if not entry["generation_unchanged"]:
            fail("{!r} restarted the qualification owner (generation changed)".format(command))
        if not entry["paused_unchanged"]:
            fail("{!r} changed the pause state".format(command))
        if entry["window_events"] != window_events_before:
            fail("{!r} changed window events ({} -> {})".format(
                command, window_events_before, entry["window_events"]))
    notes["shortcut_refusals"] = refusals
    notes["baseline_state"] = baseline
    notes["post_refusal_state"] = refresh_state(app)
    # Captured before close: the app's session id must be known while it is alive.
    app_pid = app["proc"].pid
    app_identity = proc_identity(app_pid)
    app_session = app_identity[2] if app_identity else None
    notes["app_identity"] = {"pid": app_pid, "ppid_pgrp_session": app_identity}

    send(app, "application-close")
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline and app["proc"].poll() is None:
        pump(app, 0.2)
    exit_code = app["proc"].wait(timeout=10)
    pump(app, 0.5)
    notes["exit_code"] = exit_code
    notes["stdin_eof"] = app["eof"]
    if exit_code != 0:
        fail("qualification close exited {}, not 0".format(exit_code))
    survivors = survivors_of(root, app_pid, app_session)
    notes["survivors"] = survivors
    if survivors:
        fail("surviving candidate processes after close: {}".format(survivors))
    teardown = teardown_chain(app)
    notes["teardown"] = teardown
    if not teardown["complete"]:
        fail("mandatory teardown chain incomplete/out of order: {}".format(teardown))


def sanitization_findings(text, root, plugin_class):
    """Bounds the refusal output must respect (no raw path, no controls, bounded detail).

    The native startup prints the plugin safe detail as its own ``furami:`` line;
    that payload is bounded to 512 characters. Constant detail text legitimately
    contains no ``<media-prefix>``, so no substitution claim is asserted.
    """
    findings = {}
    if str(root) in text:
        findings["raw_path"] = "absolute extraction path appears in output"
    controls = sorted({repr(ch) for ch in text if not ch.isprintable() and ch != "\n"})
    if controls:
        findings["controls"] = controls
    if "\r" in text:
        findings["carriage_return"] = "CR present in refusal output"
    details = [line[len("furami: "):] for line in text.split("\n")
               if line.startswith("furami: ") and "startup failed" not in line]
    class_prefix = plugin_class + ": "
    if len(details) != 1 or not details[0].startswith(class_prefix) \
            or not details[0][len(class_prefix):].strip():
        findings["safe_detail_class"] = "expected one nonempty {} detail".format(plugin_class)
    invalid = [repr(payload) for payload in details
               if not payload.strip() or len(payload) > 512 or not payload.isprintable()]
    if invalid:
        findings["safe_detail_invalid"] = invalid
    return findings


def run_refusal(root, env, label, expect, receipt, timeout=60):
    app = None
    try:
        app = spawn_app(root, env)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline and app["proc"].poll() is None:
            pump(app, 0.2)
        if app["proc"].poll() is None:
            fail("{}: refusal did not terminate within {}s".format(label, timeout))
    except Exception as error:
        receipt["error"] = repr(error)
        raise
    finally:
        if app is not None:
            receipt["cleanup"] = finish_app(app, root, label)
            text = "\n".join(app["rows"])
            exit_code = receipt["cleanup"].get("exit_code")
            window_created = bool(event_records(app, "native_root"))
            # Require the typed startup refusal, not a matching word in another log.
            codes = re.findall(r"^furami: startup failed \(([^ ()]+)(?: at [^()\n]+)?\)$",
                               text, re.MULTILINE)
            receipt.update({"exit_code": exit_code, "window_created": window_created,
                            "expected": list(expect), "codes": codes,
                            "tail": app["rows"][-30:]})
            if type(exit_code) is not int or exit_code <= 0:
                fail("{}: expected a reaped nonzero refusal without signal, got {}".format(
                    label, exit_code))
            if window_created:
                fail("{}: a window was created before the refusal".format(label))
            if not set(codes) & set(expect):
                fail("{}: typed refusal named none of {} (found {})".format(
                    label, list(expect), codes))
            if any(code.startswith("qt_plugin_") for code in expect):
                findings = sanitization_findings(text, root, receipt["plugin_class"])
                receipt["sanitization"] = findings
                if findings:
                    fail("{}: refusal detail violates sanitization bounds: {}".format(
                        label, findings))
            store_text("refusal-{}.log".format(label), text)
        store("refusal-{}.json".format(label), receipt)


def scenario_refusal(root, home, label, relative, mode, expect):
    """Mutate one path, prove refusal/cleanup, and restore the exact original."""
    target = root / relative
    backup = target.with_name(target.name + ".smoke-bak")
    if not target.exists():
        fail("{}: mutation target absent: {}".format(label, relative))
        store("refusal-{}.json".format(label), {"error": "mutation target absent",
                                              "relative": relative})
        return
    before = target_fingerprint(target)
    case_home = make_private_root(home / "failure-{}".format(label))
    target.rename(backup)
    try:
        if mode == "invalid":
            target.write_bytes(b"not-an-elf-qt-plugin\n")
        run_refusal(root, private_environment(case_home), label, expect,
                    {"mutation": mode, "relative": relative, "home": str(case_home),
                     "plugin_class": {
                         "libqxcb.so": "platform",
                         "libqxcb-egl-integration.so": "xcb-egl",
                         "libqxcb-glx-integration.so": "xcb-glx",
                         "libqsvg.so": "svg-image",
                         "libqsvgicon.so": "svg-icon",
                     }.get(target.name)})
    finally:
        if target.exists() and mode == "invalid":
            target.unlink()
        backup.rename(target)
        after = target_fingerprint(target)
        if after != before:
            fail("{}: mutated path was not restored exactly: {} -> {}".format(
                label, before, after))


def scenario_no_display(root, home):
    case_home = make_private_root(home / "failure-no-display")
    env = private_environment(case_home)
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)
    run_refusal(root, env, "no-display", ("NoDisplay", "InvalidDisplayEnvironment"),
                {"home": str(case_home)}, timeout=30)


def main():
    global RADEON_ICD
    home, sentinel = prepare_private_roots()
    sentinel_before = sentinel_state(sentinel)
    vulkan = run_vulkan_facts()
    store("environment.json", {
        "home": str(home), "sentinel": sentinel_before, "vulkan": vulkan,
        "display": os.environ.get("DISPLAY"), "wayland": os.environ.get("WAYLAND_DISPLAY"),
        "audio_device_present": Path("/dev/snd").exists(),
        "capture_nodes": sorted(p.name for p in Path("/dev").glob("video*")),
        "gpu_nodes": sorted(p.name for p in Path("/dev/dri").glob("*")
                            if Path("/dev/dri").is_dir()),
    })
    if Path("/dev/snd").exists():
        fail("container unexpectedly exposes /dev/snd")
    if list(Path("/dev").glob("video*")):
        fail("container unexpectedly exposes a capture node")
    for program, found in vulkan["providers_found"].items():
        if found:
            fail("qualification image unexpectedly ships {}: {}".format(program, found))
    RADEON_ICD = vulkan["radeon_icd"]
    if not RADEON_ICD:
        block("no Radeon Vulkan ICD json in the container: {}".format(vulkan["icd_dirs"]))
    else:
        restricted = vulkan.get("restricted_device_names") or []
        if len(restricted) != 1 or not ("AMD" in restricted[0] or "RADV" in restricted[0]):
            block("vulkaninfo pinned to {} did not enumerate exactly one AMD device: {} "
                  "(exit {})".format(RADEON_ICD, restricted, vulkan.get("restricted_vulkaninfo_exit")))
    if BLOCKERS:
        store("summary.json", {"ok": False, "failures": FAILURES, "blocked": BLOCKERS,
                               "notes": NOTES})
        return 1
    root = extract_candidate(home)
    tree_before = tree_fingerprint(root)
    scenario_main(home, root)
    for relative in XCB_PLUGINS:
        leaf = relative.rsplit("/", 1)[-1]
        scenario_refusal(root, home, "missing-" + leaf, relative, "missing", ("qt_plugin_missing",))
        scenario_refusal(root, home, "invalid-" + leaf, relative, "invalid", ("qt_plugin_invalid",))
    scenario_no_display(root, home)
    scenario_refusal(root, home, "missing-qml", REQUIRED_QML, "missing", ("NativeLaunch",))
    final_survivors = []
    for child in APP_SESSIONS:
        identity = child["identity"]
        found = survivors_of(Path(child["root"]), child["pid"],
                             identity[2] if identity else child["pid"])
        final_survivors.append({"child": child, "survivors": found})
        if found:
            fail("candidate survivors at final recorded-session scan: {}".format(found))
    NOTES["final_survivor_scan"] = final_survivors
    tree_after = tree_fingerprint(root)
    NOTES["tree_fingerprint_before"] = tree_before
    NOTES["tree_fingerprint_after"] = tree_after
    if tree_before != tree_after:
        fail("extracted tree was not restored between refusal cases: {} -> {}".format(
            tree_before, tree_after))
    sentinel_after = sentinel_state(sentinel)
    NOTES["sentinel_before"] = sentinel_before
    NOTES["sentinel_after"] = sentinel_after
    if sentinel_after != sentinel_before:
        fail("settings sentinel changed across the whole run: {} -> {}".format(
            sentinel_before, sentinel_after))
    store("summary.json", {"ok": not FAILURES and not BLOCKERS, "failures": FAILURES,
                           "blocked": BLOCKERS, "notes": NOTES})
    return 1 if (FAILURES or BLOCKERS) else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except Exception as error:
        try:
            import traceback
            store("fatal.json", {"error": repr(error), "traceback": traceback.format_exc(),
                                 "failures": FAILURES, "blocked": BLOCKERS, "notes": NOTES})
        except Exception:
            pass
        sys.stderr.write("FATAL: {!r}\n".format(error))
        sys.exit(3)
'''


if __name__ == "__main__":
    raise SystemExit(main())
