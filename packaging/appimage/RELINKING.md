# Replacing libraries and rebuilding Furami

Furami-authored source is MIT. The distributed combined executable uses GPL-compatible terms with the selected Qt/media/runtime terms recorded in `components.json` and original notices in `NOTICE.txt`. `release-ready.json` is the source/license completeness gate. A non-ready local smoke artifact is not a cleared release or a complete Corresponding Source offer.

## Obtain and check source materials

Keep the AppImage and all adjacent release assets together. Verify `SHA256SUMS` before extracting materials. Each `source-materials-###.tar` is independently extractable into the same empty directory:

```sh
mkdir recipient-inputs
for archive in source-materials-*.tar; do
  tar -xf "$archive" -C recipient-inputs
done
```

`sources-index.json` lists every supplied file and its SHA256, plus actual missing required inputs and separately optional unavailable sources. The material set is complete only when the release gate says it is. Missing files are never represented by empty archives or future source-offer promises.

The extracted tree contains:

- `furami/`, the exact authored Rust, C++, headers, QML, Cargo files, build script, toolchain pin, license, verified Cargo vendor source and packaging recipes/templates/icons.
- `app-inputs/`, checked Rust distribution and Cargo crate inputs.
- `media-inputs/`, available frozen mpv/FFmpeg/libplacebo/Vulkan-Headers inputs and the verified official Qt build kit.
- `qt-inputs/` and `qt-evidence/`, applicable checked Qt source archives, vendor source SPDX, third-party license evidence and original configure options/summaries. `sources-index.json.qt_correspondence` records source/code/license checksum reconciliation against vendor source SPDX. This does not claim a bit-identical Qt rebuild.
- `ubuntu-inputs/` and `ubuntu-evidence/`, applicable exact source descriptors/original source/Ubuntu patches, signed-source-index evidence and original component copyright notices.
- `runtime-inputs/` and `runtime-evidence/`, available type-2 runtime/FUSE/squashfuse sources and patch, locked rebuild inputs and original static contributor notices/linkage receipts.

Plain checked HTTPS acquisition remains in the recipes, but local developer cache paths are not recipient instructions. A required unavailable historical source/build input blocks publication.

## Replace shared libraries without rebuilding the app

Qt, libmpv, libplacebo, FFmpeg and bundled application dependencies are ordinary separate ELF shared libraries under `usr/lib/`. Extract the image without FUSE:

```sh
./Furami-0.1.0-x86_64.AppImage --appimage-extract
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

The original sources can build the original application. For modified source, adjust the relevant hash/pin deliberately in your own copy rather than disabling verification of unrelated inputs. Full application source and verified vendor crates permit relinking static CXX/Rust support code too. The recipe does not contact a publication service.

## Replace statically linked runtime libfuse

The outer type-2 runtime statically links LGPL-2.1 libfuse. Replacing an AppDir shared library does not replace this code. Modify the supplied unpatched FUSE source, preserve interface compatibility, create a source tarball and invoke the runtime recipe:

```sh
python3 recipient-inputs/furami/packaging/appimage-runtime/build.py \
  /absolute/new-modified-runtime \
  --source-cache /absolute/recipient-inputs/runtime-inputs \
  --modified-libfuse /absolute/modified-fuse.tar.xz
```

The recipe applies the exact frozen mount patch and relinks the complete runtime. Modified-libfuse mode records the new source hash and keeps unrelated APK/source pins checked. Permissive/exception source copies for unchanged compiler/static inputs are optional; the checked binary toolchain, signed historical indexes and required FUSE source/relink inputs still must be reachable. It does not claim a new modified runtime is the original canonical provider.

To repack a modified extracted tree, use the pinned appimagetool 1.9.1 asset and a compatible source-built runtime explicitly with `--runtime-file`. Its official asset URL/hash are recorded in the build receipt and `stage.py`; no implicit latest runtime is used:

```sh
/path/to/pinned-appimagetool --appimage-extract
ARCH=x86_64 ./squashfs-root/AppRun --no-appstream \
  --runtime-file /absolute/new-modified-runtime/runtime-x86_64 \
  /absolute/modified-AppDir /absolute/Furami-modified-x86_64.AppImage
```

Extract appimagetool in a separate directory so its `squashfs-root` does not overwrite Furami's extracted tree. The modified artifact needs its own audit/checksums and honest provenance before redistribution. These instructions describe the replacement route; they do not claim a modified-library/runtime rebuild was exercised on your machine.
