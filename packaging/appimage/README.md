# Furami AppImage recipe

Builds the current x86_64 Furami application against the frozen Ubuntu 24.04 media and Qt providers. The host needs Python 3.11+, rootless Podman, binutils `readelf`, HTTPS access and free space for the locked inputs and build outputs. No host Qt, mpv or FFmpeg is used to compile the application.

## Build one image

```sh
python3 packaging/appimage/build.py --output /absolute/new-output --jobs 2
```

`--version` defaults to `Cargo.toml` and, when supplied, must match its package version. Output must be a new absolute directory outside the checkout. Jobs are bounded to 1–4, default 2. The cold path calls the existing media and type-2 runtime builders, compiles the application once, and invokes pinned appimagetool once. It does not publish anything.

For a resource-bounded local run, reuse verified provider roots and checked input caches:

```sh
python3 packaging/appimage/build.py \
  --output /absolute/new-output \
  --media-output /absolute/media-build \
  --runtime-build /absolute/runtime-build \
  --source-cache /absolute/app-input-cache \
  --media-source-cache /absolute/media-input-cache \
  --runtime-source-cache /absolute/runtime-input-cache \
  --tool-cache /absolute/appimagetool-cache \
  --jobs 2
```

Media input is a build root containing `manifest.json`, `prefix/` and the locked APT receipts, not just a prefix. Runtime input is a build root containing `manifest.json`, `runtime-x86_64`, notices and relink materials, not just a binary. Every declared input artifact is checked against its frozen lock/receipt. Modified-libfuse demonstration outputs are rejected as canonical providers.

`--acquire-sources` explicitly requests missing checked source/relink inputs. Omit it for the local smoke image: unavailable large source inputs are recorded, not silently downloaded. Cold CI release builds use it. `--compliance-cache` controls the corresponding-source cache; `--source-cache` controls pinned Rust/Cargo crate inputs. Download cache entries are rechecked before reuse.

Each fresh application build writes `app/build-inputs.json` and `app/source-snapshot/` before compilation. They bind current source files, build recipes, locked source archives and media receipt. Sources, snapshot hashes and frozen media bytes are rechecked after compilation. `app/first/evidence/container-command.json` records the exact build invocation; Qt tool hashes and RCC calls have separate receipts.

## Frozen qmake and reproducible RCC

`QMAKE` is the actual `/out/prefix/bin/qmake6`, not a wrapper. Furami's existing `build.rs` guard verifies its canonical identity against the frozen prefix. The earlier qmake wrapper changed that identity and correctly failed the guard.

Locked `qt-build-utils` 0.10.0 clears the environment for both RCC compilation and listing, without forwarding `QT_RCC_SOURCE_DATE_OVERRIDE` or `SOURCE_DATE_EPOCH`. Its qmake-backed discovery exposes no RCC path override. See [the pinned RCC implementation](https://github.com/KDAB/cxx-qt/blob/v0.10.0/crates/qt-build-utils/src/tool/rcc.rs). Normalizing only source mtimes would not cover build-generated resources.

The recipe uses one read-only container file bind over the RCC discovery path. `rcc-wrapper.sh` embeds the locked epoch, logs the exact arguments, then executes the unchanged original RCC through a second read-only provider mount. Original RCC retains its relative library layout and `$ORIGIN/../lib` RUNPATH. The host prefix, qmake queries, vendored crates and runtime library bytes remain unchanged. [Qt 6.11.2 RCC](https://github.com/qt/qtbase/blob/v6.11.2/src/tools/rcc/rcc.cpp) supports both epoch variables for resource timestamps. `first/evidence/qt-build-tools.json` records qmake, original RCC and wrapper hashes; `rcc-commands.txt` records actual calls.

## Output and publication boundary

`OUTPUT/release/` contains:

- `Furami-<version>-x86_64.AppImage` and `SHA256SUMS`.
- `manifest.json`, binding the final image hash, independently extracted file/mode/symlink manifest, owners and outer runtime.
- `audit.json`, recording ELF dependencies, interpreter and GLIBC/GLIBCXX/CXXABI requirements, known `dlopen` roots, QML scanner closure and static contributor evidence.
- `components.json` and `NOTICE.txt`, retaining original Cargo, Ubuntu, Qt and runtime license provenance. Every actual file has a component mapping; unresolved applicable terms are not replaced with guessed licenses.
- `sources-index.json`, available checked source/material files, missing required inputs and optional unavailable sources.
- Independently extractable `source-materials-###.tar` files, containing only real available bytes. These are not described as complete Corresponding Source when the index says otherwise.
- `RELINKING.md` and `release-ready.json`.

`release-ready.json.ready=false` means **not publishable**, even if the image runs and the technical audit passes. Its reasons name missing required source/relink inputs or unresolved actual component terms. A successful local artifact build is not release clearance or multi-distribution GUI qualification. Permissive-only source absence is reported separately; original required notices are retained. The combined distribution uses a GPL-compatible route, while Furami-authored source remains MIT.

`OUTPUT/provenance.json` locates the exact app, media, runtime and stage receipts. Build logs remain under those outputs. Technical audit failures are fatal and leave the image/receipts available for diagnosis; they do not create a successful release directory. Rerun the independent audit on the existing image without rebuilding it:

```sh
python3 packaging/appimage/audit.py \
  --image /absolute/new-output/release/Furami-0.1.0-x86_64.AppImage \
  --provenance /absolute/new-output/provenance.json \
  --output /absolute/new-audit
```

If the original audit failed before delivery, the image remains under `OUTPUT/stage/` instead of `release/`. Use that path. Auditing extracts image bytes into the new audit output, not into the checkout.

`audit.py` is the supported independent audit command. Its internal `inventory.py` helpers inspect extracted files, ELF search contexts and static link evidence; they do not resolve dependencies against libraries installed on the audit host.

## Runtime layout

`AppRun` preserves the working directory and argument vector. It sets `FURAMI_MEDIA_PREFIX` to its own `usr/`, replaces inherited `LD_LIBRARY_PATH` and Qt/QML lookup paths, selects bundled Basic Controls and xcb, and clears Qt theme/style/plugin injection variables. User session, configuration, audio and deliberate loader/Vulkan diagnostic variables remain intact. This is path isolation, not a security sandbox.

Qt, QML, libmpv and other application libraries are bundled. X11/XCB/xkbcommon libraries come from the pinned application closure. An exact allowlist keeps glibc/loader and host Vulkan/GL/EGL/DRM/GBM providers outside the image; GPU drivers and ICD manifests are never bundled. See [portability prerequisites](PORTABILITY.md).

FUSE-free extraction is supported by the type-2 runtime:

```sh
./Furami-0.1.0-x86_64.AppImage --appimage-extract
./squashfs-root/AppRun
```

Extraction does not prove a no-FUSE host qualification. Direct launch and extracted launch both require a reachable X11/XWayland connection and usable host Vulkan stack.

## CI reproducibility comparison

```sh
python3 packaging/appimage/build-twice.py \
  --output /absolute/new-comparison --acquire-sources --jobs 2
```

This CI-only path performs two complete cold builds in fresh containers and output roots. Only hash-verified downloads are shared. It compares actual image bytes, extracted content/modes/symlinks, source runtime, generated C++/headers and actual source-material archives. Unexplained executable, payload or source differences fail.

The comparator reuses the runtime recipe's narrow proof for Clang temporary-object names in raw link-map metadata and the derived manifest checksum. It also records the pinned appimagetool's known 16-byte `.digest_md5` metadata write if that is the only image difference. Original bytes and hashes are retained. Such a result is `documented_metadata_variance`, with `byte_identity=false`, never `identical`. No executable instructions or payload bytes are normalized. Passing comparison exposes the second candidate at `OUTPUT/release/` and adds `reproducibility.json` to its checksum set.

AppImage CI is separate from ordinary checks: explicit manual dispatch creates an unpublished artifact; release tags use the complete comparison/source gate and separately protected publication job. No branch-push or pull-request packaging is configured.
