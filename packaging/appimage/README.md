# Furami AppImage recipe

Builds the current x86_64 Furami application against the frozen Ubuntu 24.04 media and Qt providers. The host needs Python 3.11+, rootless Podman, binutils `readelf`, HTTPS access and free space for the locked inputs and build outputs. No host Qt, mpv or FFmpeg is used to compile the application.

## Source-only reassembly of retained cold builds

```sh
python3 packaging/appimage/build-twice.py --reassemble-from /absolute/original-cold-pair \
  --output /absolute/new-source-assembly --acquire-sources
```

The parent has independent `first/` and `second/` cold outputs. This route invokes no compiler or provider builder: it verifies retained providers and unchanged application inputs, independently assembles sources from each parent's recorded inputs, copies unchanged images, and uses the existing comparator and readiness gate. Output must be fresh and disjoint from the immutable parent. Each side retains original provenance and a separate `source-reassembly.json`; the comparison explicitly records staged evidence. A retained original image is read in place under its historical name and is never renamed or rewritten, while every freshly assembled release is named with the current canonical basename. This proves neither two new cold builds nor execution of the updated full release recipe. A successful offline candidate check alone cannot prove either claim.

Recipient materials contain two explicitly derived `media-evidence/*.loader-resolution.txt` reports instead of the two raw media `ldd` transcripts. Only recognized terminal glibc process addresses are removed; dependency paths, names and ordering remain. Raw transcripts stay in the original local providers. Raw-to-derived hashes and assembler identity are retained in local `source-assets-local-assembly.json`, outside release assets.

The exact eight qtbase Git metadata files omitted from the official archive are supplied separately under `qt-inputs/qtbase-git-metadata/`, with original relative paths, pinned commit and SHA1/SHA256 checks. Root and tinycbor REUSE annotations, BSD and MIT notices accompany them. The official archive is unchanged; overlay this subtree onto its extracted qtbase root. Offline verification checks actual archive/SPDX/supplement bytes, not only a success receipt.

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

- `Furami-v<version>-Linux-x64.AppImage` and `SHA256SUMS`.
- `manifest.json`, binding the final image hash, independently extracted file/mode/symlink manifest, owners and outer runtime.
- `audit.json`, recording ELF dependencies, interpreter and GLIBC/GLIBCXX/CXXABI requirements, known `dlopen` roots, QML scanner closure and static contributor evidence.
- `components.json` and `NOTICE.txt`, retaining original Cargo, Ubuntu, Qt and runtime license provenance. Every actual file has a component mapping; unresolved applicable terms are not replaced with guessed licenses.
- `sources-index.json`, available checked source/material files, missing required inputs and optional unavailable sources.
- Independently extractable `source-materials-###.tar` files, containing only real available bytes. These are not described as complete Corresponding Source when the index says otherwise.
- `RELINKING.md` and `release-ready.json`.

`release-ready.json.ready=false` means **not publishable**, even if the image runs and the technical audit passes. Its reasons name missing required source/relink inputs or unresolved actual component terms. A successful local artifact build is not release clearance or multi-distribution GUI qualification. Permissive-only source absence is reported separately; original required notices are retained. The combined distribution uses a GPL-compatible route, while Furami-authored source remains MIT.

Two license determinations apply to the candidate chain and are recorded as data, not as guessed licenses:

- Every pinned Ubuntu package of the application closure, permissive ones included, is covered by `ubuntu-terms.lock.json` and requires its corresponding source. The lock pins each binary/source identity, notice SHA-256, retained license expression and exception, and the shipped objects attributed to it; a missing entry, a changed notice hash or an unexplained object is a refusal, never a fallback to a text heuristic.
- Built-in QML generated metadata inherits the terms of its backing module. The file's own `license_expression` stays `null`; `selected_terms` records `LGPL-3.0-only` only when the module/backing target and URI, its SBOM and the recorded `qmldir`, generated-file and CMake-export hashes all agree, under the documented `qt-qml-metadata-inherits-module-terms` rule. This is Furami's user-authorized reading of the module's declared terms — it is not a per-file Qt attestation, and a missing module term, backend or hash leaves the file unresolved instead.

### Public material scope

The public material set carries corresponding sources, not build tooling binaries. It holds the Furami application tree (Rust, C++, headers, QML, packaging recipes and Cargo inputs), the frozen media and verified Qt sources, the runtime/FUSE sources and patch, the full linked Alpine source closure (each pinned `APKBUILD` with its upstream and patch inputs), the recipes/locks, the original notices and the provenance receipts.

Alpine **binary** bootstrap packages and base-image material are deliberately excluded: they are not publicly distributed, they stay in a private authenticated cache outside the candidate and CI upload roots, and they are not recorded as license-cleared. The public assets therefore are **not** a complete offline bootstrap of the AppImage runtime — a recipient needs those pinned binary inputs to rebuild the runtime. The candidate records this policy as data in its sources index (`material_policy` with `alpine_bootstrap: "private"` and `public_offline_bootstrap_complete: false`) and lists exactly which checked files are supplied; signed index metadata stays separate and never substitutes for binary or source coverage, and an absent actual public corresponding source still blocks readiness rather than being replaced by a private-cache reference.

`OUTPUT/provenance.json` locates the exact app, media, runtime and stage receipts. Build logs remain under those outputs. Technical audit failures are fatal and leave the image/receipts available for diagnosis; they do not create a successful release directory. Rerun the independent audit on the existing image without rebuilding it:

```sh
python3 packaging/appimage/audit.py \
  --image /absolute/new-output/release/Furami-v0.1.0-rc.1-Linux-x64.AppImage \
  --provenance /absolute/new-output/provenance.json \
  --output /absolute/new-audit
```

If the original audit failed before delivery, the image remains under `OUTPUT/stage/` instead of `release/`. Use that path. Auditing extracts image bytes into the new audit output, not into the checkout.

`audit.py` is the supported independent audit command. Its internal `inventory.py` helpers inspect extracted files, ELF search contexts and static link evidence; they do not resolve dependencies against libraries installed on the audit host.

## Runtime layout

`AppRun` preserves the working directory and argument vector. It sets `FURAMI_MEDIA_PREFIX` to its own `usr/`, replaces inherited `LD_LIBRARY_PATH` and Qt/QML lookup paths, selects bundled Basic Controls and xcb, and clears Qt theme/style/plugin injection variables. User session, configuration, audio and deliberate loader/Vulkan diagnostic variables remain intact. This is path isolation, not a security sandbox.

Qt, QML, libmpv and other application libraries are bundled. X11/XCB/xkbcommon libraries come from the pinned application closure. An exact allowlist keeps glibc/loader and host Vulkan/GL/EGL/DRM/GBM providers outside the image; GPU drivers and ICD manifests are never bundled. See [portability prerequisites](PORTABILITY.md).

The media provider's own `mpv` program is staged at `usr/bin/mpv` from the same frozen prefix manifest as its shared libraries, and enters the same recursive ELF closure/RPATH relocation and audit as every other shipped object. That is the program the renderer qualification runs through `FURAMI_MEDIA_PREFIX`; no other provider program (`bin/ffmpeg`, `bin/ffprobe`) is shipped.

FUSE-free extraction is supported by the type-2 runtime:

```sh
./Furami-v0.1.0-rc.1-Linux-x64.AppImage --appimage-extract
./squashfs-root/AppRun
```

Extraction does not prove a no-FUSE host qualification. Direct launch and extracted launch both require a reachable X11/XWayland connection and usable host Vulkan stack.

## Release channels

Furami is distributed through tag-driven GitHub releases. A stable tag is `vX.Y.Z` and becomes a GitHub release marked **Latest**; a candidate tag is `vX.Y.Z-rc.N` with `N >= 1` and no leading zero and becomes a pre-release that is never Latest. Either tag must equal `v` followed by the `Cargo.toml` package version of its own commit — a tag never rewrites the version, and the Cargo version itself has no leading `v`. The published asset is named `Furami-v<version>-Linux-x64.AppImage`, so the current version produces `Furami-v0.1.0-rc.1-Linux-x64.AppImage` and a stable `0.1.0` would produce `Furami-v0.1.0-Linux-x64.AppImage`. The workflow triggers on `v*` tags while the executable guard derives the channel from the tag itself, so a malformed tag is refused before the build and manual dispatch stays artifact-only.

A stable release therefore needs its version prepared in the tagged commit: set `Cargo.toml` and its single `Furami` `Cargo.lock` entry to the stable version and add the exact `## [<version>] - <date>` section to `CHANGELOG.md`, then commit before tagging. The release notes are extracted from that same tagged commit, never the working tree. Both channels keep the full source/license/checksum/reproducibility gate; a stable tag gets no relaxation. The asset name matches GearLever's `Furami-v*-Linux-x64.AppImage` pattern, but the image carries no embedded update information and ships no `.zsync`, so naming alone does not configure automatic updates — such a tool reads `/releases/latest`, which only a stable release is marked as.

Every published image, stable or pre-release, keeps the `release-ready.json.ready == true` source/license gate; unresolved component terms or missing corresponding-source inputs keep it unpublished. The AppImage and its adjacent assets stay together: `SHA256SUMS`, `manifest.json`, `audit.json`, `components.json` + `NOTICE.txt`, `sources-index.json`, the `source-materials-*.tar` archives, `reproducibility.json` and [RELINKING.md](RELINKING.md). The adjacent archives are the candidate's source offer; `sources-index.json` states what is actually supplied, and no empty archive or future offer stands in for a missing file. The offer is corresponding sources, not the Alpine binary bootstrap: the private bootstrap packages and base image are excluded from it (see [Public material scope](#public-material-scope)).

Verify a candidate locally without network access, a token or a real remote tag:

```sh
python3 scripts/publish_candidate.py --check-only --require-reproducibility \
  --event-name push --tag v0.1.0-rc.1 --candidate /absolute/new-output/release
```

The same check accepts a prepared stable tag (`--tag v0.1.0`): the channel follows the tag, so nothing else changes. `--check-only` reads the version from `Cargo.toml`, requires the matching Furami entry in `Cargo.lock`, and re-verifies the real files: a non-empty exact image, regular non-symlink assets, a `SHA256SUMS` that covers every upload except itself with no duplicate or dangerous path, `release-ready.json.ready is True` with image/audit/sources-index fingerprints matching the actual bytes, and `reproducibility.json` linked to the selected image by `second_image_sha256` with accepted comparison criteria. It writes nothing and returns the sorted list of verified assets. Publication is a separate, explicit run (`publish_candidate.py` without `--check-only`) performed by the tag workflow's protected job; it never happens during verification.

## Qualification video

The candidate chain can exercise the real Furami window without a capture card, using a deterministic local video instead of a UVC source. Generate the bundled fixture and pass it to the application:

```sh
python3 packaging/appimage/qualification_fixture.py /absolute/qualification.y4m
./squashfs-root/AppRun --qualification-video /absolute/qualification.y4m --qualification-stdin
```

`--qualification-video` takes one absolute, regular, readable Y4M file; relative paths, URLs, playlists, FIFOs and combinations with capture options are refused before any capture, audio or saved setting is touched. The generator writes 120 frames of 320x180 at 30/1 with a moving rectangle and a per-frame binary index, so a paused frame is provably identical across captures; it uses only the Python standard library, with no clock, randomness, fonts or encoder. The packaged smoke uses this route inside an isolated Fedora 44 / KWin Wayland container (`qualification-fedora44.Containerfile`, `smoke-fedora44.py`) with a private HOME/XDG tree and no host session, audio or UVC access.

## CI reproducibility comparison

```sh
python3 packaging/appimage/build-twice.py \
  --output /absolute/new-comparison --acquire-sources --jobs 2
```

This CI-only path performs two complete cold builds in fresh containers and output roots. Only hash-verified downloads are shared. It compares actual image bytes, extracted content/modes/symlinks, source runtime, generated C++/headers and actual source-material archives. Unexplained executable, payload or source differences fail.

The comparator reuses the runtime recipe's narrow proof for Clang temporary-object names in raw link-map metadata and the derived manifest checksum. It also records the pinned appimagetool's known 16-byte `.digest_md5` metadata write if that is the only image difference. Original bytes and hashes are retained. Such a result is `documented_metadata_variance`, with `byte_identity=false`, never `identical`. No executable instructions or payload bytes are normalized. Passing comparison exposes the second candidate at `OUTPUT/release/` and adds `reproducibility.json` to its checksum set.

AppImage CI is separate from ordinary checks: explicit manual dispatch creates an unpublished artifact only; pushed `v*` tags run the complete comparison/source gate with a separately protected publication job, and the executable guard accepts stable and pre-release tags while refusing malformed ones — stable or not. The publication job serializes through a built-in `queue: max` concurrency group (up to 100 pending jobs) so simultaneous release writers do not race; that is FIFO by waiting time, not SemVer order, and the last stable publication marked Latest is the one GitHub serves. No branch-push or pull-request packaging is configured. Because a tag pushed on a workflow that is not yet on the default `develop` branch runs with a restricted `GITHUB_TOKEN`, merge the workflow into `develop` before pushing a release tag; broadening token permissions or adding a personal access token is not a substitute.
