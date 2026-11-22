# Frozen media source build

`stack.lock.json` freezes Ubuntu 24.04 x86_64 by OCI digest and a signed APT snapshot. `build-apt-closure.json` retains the same 408 installed packages and 317 downloaded `.deb` SHA256 hashes. The producer locks the CA bootstrap package, mpv 0.41.0 (client API 2.5.0), libplacebo 7.360.1, its Vulkan-Headers gitlink, and FFmpeg 8.1.2 with its ordered source patch. The gitlink supplies Vulkan 1.4 headers absent from Noble. There is no media system-package fallback, nonfree FFmpeg configuration, host-root installation, or submodule network fetch. The producer does not fetch, install or attest a Qt kit or CXX-Qt source.

`sources.mpv.build_flags` freezes `-Djpeg=disabled`. This drops mpv's optional libjpeg image writer, not FFmpeg's `libavcodec` MJPEG decoder needed for capture. The container records `readelf --dynamic` for `mpv` and `libmpv.so` and fails if either directly needs `libjpeg.so`; the normal decoder, VO, context and filter checks remain mandatory. An earlier compiled candidate had a direct `libjpeg.so.8` dependency and is not the post-freeze deliverable.

On a clean checkout with Python 3, working rootless Podman, network access and enough disk/RAM for the media builds:

```sh
python3 packaging/media/build.py "$HOME/furami-media-run-1"
python3 packaging/media/build-twice.py "$HOME/furami-media-repro"
python3 -m unittest discover -s packaging/media -p build_tests.py
```

Use new output directories. `build.py` verifies each source archive's SHA256 before making it available on a read-only mount. It records `OUTPUT/prefix/`, media build flags, FFmpeg/Meson configuration, exact APT package versions and downloaded `.deb` SHA256, diagnostics, the applied source-patch receipt and a manifest with every delivered file/symlink hash. `build-twice.py` performs two clean container builds with a shared verified download cache, compares prefix files/symlinks and build provenance, and only then writes `reproducibility.json`. A failed build retains `build.log` and partial output but no success manifest.

`OUTPUT/prefix/bin/mpv`, `bin/ffmpeg`, `lib/libmpv.so`, `lib/libplacebo.so`, `lib/libav*.so` and their pkg-config metadata come from that private prefix. The recipe checks exact private FFmpeg/libplacebo versions and prefixes, the mpv client ABI, `gpu-next`/`x11vk` listings, FFmpeg decoder/filter listings, X11 pkg-config prerequisites and dynamic loader paths for libmpv/mpv. The device-listing check accepts input-capable `D`/`DE` rows and exact comma-separated aliases such as `video4linux2,v4l2`, not output-only devices. CLI listings alone do **not** prove live Vulkan rendering, hardware input, mpv-native `format`, lavfi bridge behavior, or legal redistribution. Hardware probe evidence remains local; final AppImage inventory is the separate FUR-005 publication gate.

Production packaging requires fresh provenance for the media-only lock. Preserve historical build-G receipts and the original Qt-capable prefix for the comparison build; they attest a different lock and cannot be reused as media-only receipts. Compare libmpv, FFmpeg and libplacebo file hashes against the pinned reference built with matching inputs, including matching FFmpeg patch bytes. Investigate discrepancies before claiming unchanged media. A single run is not evidence that two cold builds reproduce each other.

The named Ubuntu packages describe the build environment, **not** redistributed AppImage components. The [core license audit](LICENSE-AUDIT.md) separately covers static glslang/SPIRV-Tools archives linked by libplacebo.

## Building the application against this prefix with the pinned toolchain

The application is built separately (see the [AppImage documentation](../appimage/README.md)). Keep the pinned Rust 1.97.1 and GNU BFD linker for native builds:

```sh
FURAMI_MEDIA_PREFIX=/absolute/path/to/media/prefix \
RUSTC=/absolute/path/to/1.97.1/bin/rustc \
RUSTDOC=/absolute/path/to/1.97.1/bin/rustdoc \
CARGO_ENCODED_RUSTFLAGS=$'-Clinker-features=-lld\x1f-Clink-arg=-fuse-ld=bfd' \
cargo +1.97.1 build --locked
```

`cargo` must be the rustup proxy to accept `+1.97.1`. `RUSTC` and `RUSTDOC` point at the same pinned toolchain, and `\x1f` separates the two encoded flags. The GPUI build needs development files for X11/XCB, fontconfig, FreeType and xkbcommon (including xkbcommon-x11).

## Host audio helpers (runtime requirement, audio enabled only)

When audio output is enabled, Furami drives a single owned `pw-loopback` child for the capture→output transport and uses `pw-metadata` to validate the loopback node's ownership before launch. Both helpers must already be present on the host at `/usr/bin/pw-loopback` and `/usr/bin/pw-metadata`; Furami never ships, installs, downloads, or launches a PipeWire daemon, session manager, or WirePlumber — the normal user session must already provide them.

- Supported helper environment: the qualified target is PipeWire 1.6.9 with WirePlumber 0.5.18; other combinations are unsupported. A missing or unusable helper is a typed runtime failure reported to the user, never a silent fallback to another audio path and never an automatic install.
- This is a host dependency contract plus the recorded mechanical-gate result (the same helper child and capture route were exercised across EasyEffects/default/quality-mode sink loss and recreation, control, and cleanup). It is not a certification of which library paths the helper resolves at runtime; host installations may differ.
- Without audio enabled, none of these helpers are required or launched.

The AppImage qualification (FUR-005) does not bundle these helpers and does not certify audio, A/V, or release clearance.

Sources: [mpv tag](https://github.com/mpv-player/mpv/tree/v0.41.0), [libplacebo tag](https://github.com/haasn/libplacebo/tree/v7.360.1), [FFmpeg release](https://ffmpeg.org/releases/), [Ubuntu snapshot service](https://ubuntu.com/server/docs/how-to/software/snapshot-service/).
