#!/usr/bin/env bash
# Run only inside the digest-pinned rootless Ubuntu container launched by build.py.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
export TZ=UTC LC_ALL=C.UTF-8
PREFIX=/out/prefix
WORK=/out/work
mkdir -p "$WORK" "$PREFIX"

# APT verifies Canonical's signed InRelease -> Packages -> each .deb SHA256.
# The minimal Ubuntu OCI image has no CA trust store. Extract verified Noble CA
# package only for HTTPS bootstrap; APT later installs that package normally.
mkdir -p /tmp/noble-ca
dpkg-deb -x /sources/ca-certificates.deb /tmp/noble-ca
find /tmp/noble-ca/usr/share/ca-certificates/mozilla -name '*.crt' -type f -print0 |
  sort -z | xargs -0 cat > /tmp/noble-ca/bundle.pem
[[ -s /tmp/noble-ca/bundle.pem ]] || { echo 'CA bundle missing' >&2; exit 1; }
printf 'Acquire::https::CaInfo "/tmp/noble-ca/bundle.pem";\n' > /etc/apt/apt.conf.d/50-furami-ca
# Direct immutable snapshot URI, not Snapshot: on live sources. APT can also
# query the live URI with Snapshot:, which violates this recipe's source closure.
cat > /etc/apt/sources.list.d/ubuntu.sources <<EOF
Types: deb
URIs: https://snapshot.ubuntu.com/ubuntu/${APT_SNAPSHOT}
Suites: noble noble-updates noble-backports noble-security
Components: main universe
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
apt-get update -o APT::Update::Error-Mode=any
packages=(
  ca-certificates curl build-essential binutils nasm python3-py7zr lld cmake ninja-build meson pkg-config python3 python3-jinja2
  git libvulkan-dev glslang-dev libgl-dev libegl-dev libx11-dev libxext-dev libxrandr-dev
  libxss-dev libxpresent-dev libxinerama-dev libxcursor-dev libxi-dev libxrender-dev libxcb1-dev libxcb-randr0-dev
  libxcb-present-dev libxcb-shm0-dev libxcb-xfixes0-dev libxcb-xinerama0-dev libxcb-xinput-dev
  libxcb-xkb-dev libxcb-keysyms1-dev libxcb-util-dev libxcb-image0-dev libxcb-icccm4-dev
  libxcb-render0-dev libxcb-render-util0-dev libxcb-shape0-dev libxcb-sync-dev
  libxcb-cursor-dev libxkbcommon-dev libxkbcommon-x11-dev libpulse-dev libasound2-dev
  libwayland-dev wayland-protocols libdrm-dev libgbm-dev libfontconfig1-dev libfreetype-dev
  libharfbuzz-dev libass-dev libpcre2-dev libzstd-dev zlib1g-dev libdouble-conversion-dev
  libglib2.0-dev libdbus-1-dev libx11-xcb-dev libv4l-dev
)
printf '%s\n' "${packages[@]}" > /out/apt-direct-packages.txt
apt-get install --yes --no-install-recommends --download-only "${packages[@]}"
find /var/cache/apt/archives -maxdepth 1 -name '*.deb' -type f -print0 |
  sort -z | xargs -0 -r sha256sum > /out/apt-debs.sha256
apt-get install --yes --no-install-recommends "${packages[@]}"
dpkg-query -W -f='${Package}\t${Version}\t${Architecture}\n' | sort > /out/apt-installed.tsv
apt-cache policy libvulkan-dev glslang-dev libxss-dev libxpresent-dev > /out/apt-policy.txt
for pc in x11 xscrnsaver xext xpresent xrandr; do
  pkg-config --exists --print-errors "$pc" || { echo "required X11 pkg-config dependency missing: $pc" >&2; exit 1; }
  printf '%s\t%s\t%s\n' "$pc" "$(pkg-config --modversion "$pc")" "$(pkg-config --variable=pcfiledir "$pc")" >> /out/x11-pkg-config.tsv
done
python3 -c 'import py7zr; print(py7zr.__version__)' > /out/py7zr-version.txt
command -v ld.lld >/dev/null || { echo 'pinned CXX-Qt linker missing: ld.lld' >&2; exit 1; }
ld.lld --version > /out/lld-version.txt
printf '%s  %s\n' "$APT_CLOSURE_SHA256" /workspace/packaging/media/build-apt-closure.json |
  sha256sum --check --status || { echo 'named APT package closure changed' >&2; exit 1; }
printf '%s  %s\n' "$APT_INSTALLED_SHA256" /out/apt-installed.tsv |
  sha256sum --check --status || { echo 'APT package/version closure changed' >&2; exit 1; }
printf '%s  %s\n' "$APT_DEBS_SHA256" /out/apt-debs.sha256 |
  sha256sum --check --status || { echo 'APT transitive .deb SHA256 closure changed' >&2; exit 1; }

# Media sources were SHA256-verified by build.py before read-only mounting.
mkdir -p "$WORK/ffmpeg" "$WORK/libplacebo/3rdparty/Vulkan-Headers" "$WORK/mpv"
tar -xf /sources/ffmpeg.tar.xz --strip-components=1 -C "$WORK/ffmpeg"
tar -xf /sources/libplacebo.tar.gz --strip-components=1 -C "$WORK/libplacebo"
tar -xf /sources/vulkan-headers.tar.gz --strip-components=1 -C "$WORK/libplacebo/3rdparty/Vulkan-Headers"
tar -xf /sources/mpv.tar.gz --strip-components=1 -C "$WORK/mpv"
# CXX-Qt is a Cargo build input, not a Qt library. Its verified source is locked
# but Cargo's bridge/dependency closure must be resolved by the application build.

export CFLAGS="-O2 -ffile-prefix-map=$WORK=/usr/src/furami"
export CXXFLAGS="$CFLAGS"
export LDFLAGS="-Wl,-rpath,$PREFIX/lib"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
export LD_LIBRARY_PATH="$PREFIX/lib"
export SOURCE_DATE_EPOCH
JOBS="${FURAMI_BUILD_JOBS:-2}"
[[ "$JOBS" =~ ^[1-4]$ ]] || { echo 'build jobs must be 1..4' >&2; exit 1; }

record() {
  printf '%q ' "$@" >> /out/build-flags.txt
  printf '\n' >> /out/build-flags.txt
  "$@"
}
require_private_pc() {
  local name=$1 version=$2 origin actual
  actual=$(pkg-config --modversion "$name")
  origin=$(pkg-config --variable=prefix "$name")
  if [[ "$actual" != "$version" || "$origin" != "$PREFIX" ]]; then
    printf '%s: expected %s in %s, got %s in %s\n' "$name" "$version" "$PREFIX" "$actual" "$origin" >&2
    exit 1
  fi
}

# FFmpeg components required by live V4L2/Pulse input and the typed filter catalog.
# GPL is required for eq/hqdn3d; no --enable-nonfree or static output.
cd "$WORK/ffmpeg"
record ./configure --prefix="$PREFIX" --enable-shared --disable-static --disable-debug \
  --disable-doc --disable-ffplay --disable-autodetect --enable-gpl --enable-avdevice \
  --enable-indev=v4l2 --enable-libpulse --enable-indev=pulse \
  --enable-decoder=mjpeg --enable-avfilter --enable-filter=eq \
  --enable-filter=unsharp --enable-filter=hqdn3d --enable-filter=bwdif
record make -j"$JOBS"
record make install
cp ffbuild/config.mak /out/ffmpeg-config.mak
cp config.h /out/ffmpeg-config.h
require_private_pc libavcodec 62.28.102

# libplacebo's v7.360.1 gitlink pins Vulkan-Headers 1.4; Noble headers alone
# cannot satisfy VK_VERSION_1_4. No fetchable Meson wrap/subproject fallback.
cd "$WORK"
record meson setup placebo-build libplacebo --prefix="$PREFIX" --libdir=lib \
  --buildtype=release --default-library=shared --wrap-mode=nofallback \
  -Dvulkan=enabled -Dglslang=enabled -Dshaderc=disabled -Dopengl=disabled \
  -Dlcms=disabled -Dlibdovi=disabled -Ddovi=disabled -Dxxhash=disabled \
  -Ddemos=false -Dtests=false -Dunwind=disabled
record meson compile -j "$JOBS" -C placebo-build
record meson install -C placebo-build
meson introspect placebo-build --buildoptions > /out/libplacebo-buildoptions.json
require_private_pc libplacebo 7.360.1
[[ "$(pkg-config --variable=pl_has_vulkan libplacebo)" == 1 ]] || { echo 'libplacebo lacks Vulkan' >&2; exit 1; }
[[ "$(pkg-config --variable=pl_has_glslang libplacebo)" == 1 ]] || { echo 'libplacebo lacks glslang' >&2; exit 1; }
for pc in libavcodec libavfilter libavformat libavutil libavdevice libswresample libswscale; do
  [[ "$(pkg-config --variable=prefix "$pc")" == "$PREFIX" ]] || { echo "$pc fell back to system" >&2; exit 1; }
done

# mpv's upstream dependency() checks FFmpeg/libplacebo; enforce private prefix
# both before Meson and after installation. Its optional libjpeg image writer
# is not required for FFmpeg's MJPEG input decoder and must not add libjpeg.so.8.
record meson setup mpv-build mpv --prefix="$PREFIX" --libdir=lib \
  --buildtype=release --default-library=shared --wrap-mode=nofallback \
  -Dbuild-date=false -Dgpl=true -Dlibmpv=true -Dlibavdevice=enabled \
  -Dpulse=enabled -Dvulkan=enabled -Dx11=enabled -Dwayland=disabled "$MPV_JPEG_FLAG"
record meson compile -j "$JOBS" -C mpv-build
record meson install -C mpv-build
meson introspect mpv-build --buildoptions > /out/mpv-buildoptions.json
require_private_pc mpv "$MPV_CLIENT_API"
[[ -f "$PREFIX/include/mpv/client.h" ]] || { echo 'libmpv header missing' >&2; exit 1; }
[[ -e "$PREFIX/lib/libmpv.so" ]] || { echo 'libmpv shared library missing' >&2; exit 1; }

# Official Qt 6.11.2 linux_gcc_64 archives carry the original configure
# options/summaries and SPDX SBOM. No system Qt, local Qt build or aqt update.
record python3 /workspace/packaging/media/build-qt-kit.py \
  /workspace/packaging/media/stack.lock.json /sources "$PREFIX"

"$PREFIX/bin/mpv" --no-config --version > /out/mpv-version.txt
"$PREFIX/bin/ffmpeg" -hide_banner -version > /out/ffmpeg-version.txt
"$PREFIX/bin/qmake6" -query QT_VERSION > /out/qt-version.txt
"$PREFIX/bin/qmake6" -query QT_INSTALL_PREFIX > /out/qt-install-prefix.txt
"$PREFIX/bin/qmake" -query QT_INSTALL_PREFIX > /out/qt-qmake-install-prefix.txt
pkg-config --modversion Qt6Core > /out/qt-pc-version.txt
pkg-config --variable=prefix Qt6Core > /out/qt-pc-prefix.txt
[[ "$(cat /out/qt-qmake-install-prefix.txt)" == "$PREFIX" ]] || { echo 'official qmake points outside private prefix' >&2; exit 1; }
[[ "$(cat /out/qt-pc-version.txt)" == '6.11.2' && "$(cat /out/qt-pc-prefix.txt)" == "$PREFIX" ]] || {
  echo 'Qt6Core pkg-config is not the relocated official kit' >&2; exit 1;
}
[[ "$(cat /out/qt-install-prefix.txt)" == "$PREFIX" ]] || { echo 'official Qt kit is not relocatable to private prefix' >&2; exit 1; }
sed -n '1p' /out/mpv-version.txt | grep -Eq '^mpv v0\.41\.0([[:space:]]|$)' || { echo 'wrong mpv version' >&2; exit 1; }
[[ "$(sed -n '1p' /out/ffmpeg-version.txt)" == 'ffmpeg version 8.1.2'* ]] || { echo 'wrong FFmpeg version' >&2; exit 1; }
[[ "$(cat /out/qt-version.txt)" == '6.11.2' ]] || { echo 'wrong Qt version' >&2; exit 1; }
grep -Fqx '#define MPV_CLIENT_API_VERSION MPV_MAKE_VERSION(2, 5)' "$PREFIX/include/mpv/client.h"
"$PREFIX/bin/mpv" --no-config --vo=help > /out/mpv-vo.txt
"$PREFIX/bin/mpv" --no-config --gpu-context=help > /out/mpv-context.txt
grep -Eq '^[[:space:]]*gpu-next([[:space:]]|$)' /out/mpv-vo.txt
grep -Eq '^[[:space:]]*x11vk([[:space:]]|$)' /out/mpv-context.txt
"$PREFIX/bin/ffmpeg" -hide_banner -filters > /out/ffmpeg-filters.txt
for filter in eq unsharp hqdn3d bwdif; do
  grep -Eq "^[[:space:]]*[.A-Z|]+[[:space:]]+$filter[[:space:]]" /out/ffmpeg-filters.txt || { echo "FFmpeg filter missing: $filter" >&2; exit 1; }
done
"$PREFIX/bin/ffmpeg" -hide_banner -devices > /out/ffmpeg-devices.txt
bash /workspace/packaging/media/build-device-check.sh /out/ffmpeg-devices.txt
"$PREFIX/bin/ffmpeg" -hide_banner -decoders > /out/ffmpeg-decoders.txt
grep -Eq '^[[:space:]]*V[.A-Z|]+[[:space:]]+mjpeg[[:space:]]' /out/ffmpeg-decoders.txt
# Loader checks enforce media deps resolve from this prefix even if distribution
# supplies another libavcodec/libplacebo. Full AppImage DT_NEEDED audit is separate.
for library in "$PREFIX/bin/mpv" "$PREFIX/lib/libmpv.so"; do
  readelf --dynamic "$library" > "/out/$(basename "$library").dynamic.txt"
  if grep -Eq 'Shared library: \[libjpeg\.so(\.[^]]+)?\]' "/out/$(basename "$library").dynamic.txt"; then
    echo "$library: optional libjpeg image-writer dependency is forbidden" >&2
    exit 1
  fi
  ldd "$library" > "/out/$(basename "$library").ldd.txt"
  for needed in libavcodec libavfilter libavdevice libplacebo; do
    grep -E "^[[:space:]]*$needed\\.so[.0-9]* => $PREFIX/lib/" "/out/$(basename "$library").ldd.txt" >/dev/null || {
      echo "$library: $needed not loaded from $PREFIX/lib" >&2; exit 1;
    }
  done
done
