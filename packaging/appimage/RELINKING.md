# Replacing libraries and rebuilding Furami

Furami-authored source is MIT. The distributed combined executable uses GPL-compatible terms with the selected Qt/media/runtime terms recorded in `components.json` and original notices in `NOTICE.txt`. `release-ready.json` is the source/license completeness gate. A non-ready local smoke artifact is not a cleared release or a complete Corresponding Source offer.

## Releases and local verification

Furami is distributed through tag-driven GitHub releases. A stable tag `vX.Y.Z` becomes a GitHub release marked **Latest**, while a candidate tag `vX.Y.Z-rc.N` (`N >= 1`, no leading zero) becomes a pre-release that is never Latest. Either tag must equal `v` followed by the `Cargo.toml` version of its own commit; the Cargo version has no leading `v`, and the image is named `Furami-v<version>-Linux-x64.AppImage`. Publishing a stable tag requires that commit to carry the stable version in `Cargo.toml` and `Cargo.lock` plus the matching `## [<version>] - <date>` changelog section, and every image — stable or pre-release — still requires `release-ready.json.ready == true`.

Keep the image and its adjacent assets together and check them before use: `SHA256SUMS`, `manifest.json`, `audit.json`, `components.json` + `NOTICE.txt`, `sources-index.json`, the `source-materials-*.tar` archives, `reproducibility.json` and this document. `sources-index.json` states which checked inputs are actually present, and any `source-materials-*` archive extracts independently; a missing file is never replaced by an empty archive or a future offer.

Verify a candidate offline — no network, token or real remote tag — before distributing or modifying anything:

```sh
python3 scripts/publish_candidate.py --check-only --require-reproducibility \
  --event-name push --tag v1.0.0-rc.1 --candidate /absolute/candidate/release
```

A prepared stable tag (`--tag v1.0.0`) uses the same check with no other change. `--check-only` re-verifies the real files (exact non-empty image, regular non-symlink assets, a complete `SHA256SUMS`, a true readiness gate whose fingerprints match the bytes, and a `reproducibility.json` bound to that image) and lists the verified assets without writing anything. Publication is a separate, explicit run of the same script without `--check-only`, performed only by the protected tag job.

The candidate can be exercised without a capture card through the dedicated qualification video; see the [AppImage recipe](README.md#qualification-video) for the fixture generator and the `--qualification-video` entry.

## Obtain and check source materials

Keep the AppImage and all adjacent release assets together. Verify `SHA256SUMS` before extracting materials. Each `source-materials-###.tar` is independently extractable into the same empty directory:

```sh
mkdir recipient-inputs
for archive in source-materials-*.tar; do
  tar -xf "$archive" -C recipient-inputs
done
```

`sources-index.json` lists every supplied file and its SHA256, plus actual missing required inputs and separately optional unavailable sources. The material set is complete only when the release gate says it is. Missing files are never represented by empty archives or future source-offer promises.

For qtbase, the official source archive remains byte-for-byte unchanged. Copy the contents of `qt-inputs/qtbase-git-metadata/` into the extracted qtbase root, preserving relative paths. The eight Git metadata files are bound to the kit's exact upstream commit and vendor source SPDX checksums. The overlay includes original REUSE annotations, copyrights and BSD-3-Clause/MIT license texts; no individual Qt `LicenseConcluded: NOASSERTION` field is rewritten as an upstream license conclusion.

The two media loader-resolution reports are derived build-environment evidence, not raw `ldd` transcripts or a promise about recipient runtime resolution. Original address-bearing transcripts and raw-to-derived hash linkage stay in local build evidence. If `reproducibility.json.evidence_origin.mode` is `source-reassembly`, the evidence consists of retained original cold binaries plus independent new source assemblies, not execution of two updated full cold recipes. Original controlling recipes are supplied separately under `original-cold-recipes/`, alongside current assembly recipes under `furami/packaging/`.

The extracted tree contains:

- `furami/`, the exact authored Rust, C++, headers, QML, Cargo files, build script, toolchain pin, license, supplied application artwork (`assets/logo.svg`, `assets/logo.png`, `assets/logo.ico`) distributed under the user-confirmed Furami MIT terms, verified Cargo vendor source and packaging recipes/templates.
- `app-inputs/`, checked Rust distribution and Cargo crate inputs.
- `media-inputs/`, available frozen mpv/FFmpeg/libplacebo/Vulkan-Headers inputs and the verified official Qt build kit.
- `qt-inputs/` and `qt-evidence/`, applicable checked Qt source archives, vendor source SPDX, third-party license evidence and original configure options/summaries. `sources-index.json.qt_correspondence` records source/code/license checksum reconciliation against vendor source SPDX. This does not claim a bit-identical Qt rebuild.
- `ubuntu-inputs/` and `ubuntu-evidence/`, applicable exact source descriptors/original source/Ubuntu patches, signed-source-index evidence and original component copyright notices.
- `runtime-inputs/` and `runtime-evidence/`, available type-2 runtime/FUSE/squashfuse sources and patch, the linked Alpine source closure (each pinned `APKBUILD` with its upstream and patch inputs), locked rebuild inputs and original static contributor notices/linkage receipts. The Alpine binary bootstrap packages and the base-image material are not in this public set; its signed index metadata is separate and never substitutes for binary or source coverage.

Plain checked HTTPS acquisition remains in the recipes, but local developer cache paths are not recipient instructions. A required unavailable historical source/build input blocks publication.

The public set carries corresponding sources, not the Alpine binary bootstrap. The candidate records this policy as data in its sources index (`material_policy`: `alpine_bootstrap: "private"`, `public_offline_bootstrap_complete: false`). Those pinned binary packages are retained in a private authenticated cache, are not publicly distributed and are not declared license-cleared; because they are absent here, the public assets alone are **not** a complete offline bootstrap of the runtime and cannot rebuild it unaided.

## Replace shared libraries without rebuilding the app

Qt, libmpv, libplacebo, FFmpeg and bundled application dependencies are ordinary separate ELF shared libraries under `usr/lib/`. Extract the image without FUSE:

```sh
./Furami-v1.0.0-rc.1-Linux-x64.AppImage --appimage-extract
```

Replace the appropriate shared object with an ABI-compatible modified build. Preserve its SONAME and relative symlink chain. Rebuild Qt from the supplied module sources using the vendor configuration evidence when modifying Qt, and retain corresponding compatible plugins/QML. Do not mix unrelated Qt major/minor versions or host Qt plugins into the bundle.

Launch `./squashfs-root/AppRun`. It uses the extracted `usr/` as `FURAMI_MEDIA_PREFIX`, its libraries and isolated Qt/QML paths. There is no signature/hash enforcement preventing compatible replacement libraries at application startup. The original release checksums no longer describe modified bytes; do not present them as the original release.

## Rebuild the application

The source tree includes the full controlling scripts. The canonical build recipe uses frozen container/APT/source pins, not the recipient's installed Qt/mpv/FFmpeg. Set absolute input-cache paths from the extracted material set, copy the already checked `app-inputs/ca-certificates.deb` into `media-inputs/` if needed, and run:

```sh
python3 recipient-inputs/furami/packaging/appimage/build.py \
  --output /absolute/new-recipient-build \
  --source-cache /absolute/recipient-inputs/app-inputs \
  --media-source-cache /absolute/recipient-inputs/media-inputs \
  --runtime-source-cache /absolute/recipient-inputs/runtime-inputs \
  --compliance-cache /absolute/recipient-inputs/qt-inputs \
  --jobs 2
```

The original sources can build the original application. This run also rebuilds the runtime from source caches; because the pinned Alpine bootstrap binaries are not in the public material set, supply your own authenticated copies of those exact pinned inputs or reuse a runtime root you already built (`--runtime-build`) instead of relaxing a hash check. For modified source, adjust the relevant hash/pin deliberately in your own copy rather than disabling verification of unrelated inputs. Full application source and verified vendor crates permit relinking static CXX/Rust support code too. The recipe does not contact a publication service.

## Replace statically linked runtime libfuse

The outer type-2 runtime statically links LGPL-2.1 libfuse. Replacing an AppDir shared library does not replace this code. Modify the supplied unpatched FUSE source, preserve interface compatibility, create a source tarball and invoke the runtime recipe:

```sh
python3 recipient-inputs/furami/packaging/appimage-runtime/build.py \
  /absolute/new-modified-runtime \
  --source-cache /absolute/recipient-inputs/runtime-inputs \
  --modified-libfuse /absolute/modified-fuse.tar.xz
```

The recipe applies the exact frozen mount patch and relinks the complete runtime. Modified-libfuse mode records the new source hash and keeps unrelated APK/source pins checked. Permissive/exception source copies for unchanged compiler/static inputs are optional; the checked binary toolchain stays in a private authenticated cache rather than the public material set, so you must supply those pinned inputs yourself to run this recipe, while the signed historical indexes and the required FUSE source/relink inputs remain public. It does not claim a new modified runtime is the original canonical provider.

To repack a modified extracted tree, use the pinned appimagetool 1.9.1 asset and a compatible source-built runtime explicitly with `--runtime-file`. Its official asset URL/hash are recorded in the build receipt and `stage.py`; no implicit latest runtime is used:

```sh
/path/to/pinned-appimagetool --appimage-extract
ARCH=x86_64 ./squashfs-root/AppRun --no-appstream \
  --runtime-file /absolute/new-modified-runtime/runtime-x86_64 \
  /absolute/modified-AppDir /absolute/Furami-vmodified-Linux-x64.AppImage
```

Extract appimagetool in a separate directory so its `squashfs-root` does not overwrite Furami's extracted tree. The modified artifact needs its own audit/checksums and honest provenance before redistribution. These instructions describe the replacement route; they do not claim a modified-library/runtime rebuild was exercised on your machine.
