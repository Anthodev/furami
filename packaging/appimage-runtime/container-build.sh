#!/bin/sh
# Runs only inside digest-pinned Alpine, with network disabled and verified inputs.
set -eu
export LC_ALL=C TZ=UTC SOURCE_DATE_EPOCH
JOBS="${FURAMI_BUILD_JOBS:-2}"
case "$JOBS" in 1|2|3|4) ;; *) echo 'build jobs must be 1..4' >&2; exit 1 ;; esac

# The image database is pinned separately from the fetched APK closure. Alpine
# verifies signatures on both indexes and packages; --no-network also prevents
# build-system scripts from silently fetching anything.
echo "$BASE_APK_DB_SHA256  /lib/apk/db/installed" | sha256sum -c -
apk --no-network --repositories-file /sources/indexes/repositories update
apk --no-network --repositories-file /sources/indexes/repositories add --no-cache /sources/apks/*.apk
apk info -v | sort > /out/apk-installed.txt
clang --version > /out/clang-version.txt
ld --version | sed -n '1p' > /out/ld-version.txt
meson --version > /out/meson-version.txt
ninja --version > /out/ninja-version.txt

mkdir -p /out/work/runtime /out/work/fuse /out/work/squashfuse /out/licenses
tar -xf /sources/type2-runtime.tar.gz --strip-components=1 -C /out/work/runtime
tar -xf "${FUSE_ARCHIVE:-/sources/fuse-3.15.0.tar.xz}" --strip-components=1 -C /out/work/fuse
tar -xf /sources/squashfuse-0.5.2.tar.gz --strip-components=1 -C /out/work/squashfuse
cp /out/work/runtime/LICENSE /out/licenses/type2-runtime.LICENSE
# runtime.c has an additional Alexander Larsson notice absent from root LICENSE.
sed -n '1,33p' /out/work/runtime/src/runtime/runtime.c > /out/licenses/type2-runtime.runtime.c.NOTICE
cp /out/work/fuse/LICENSE /out/licenses/libfuse.LICENSE
cp /out/work/fuse/LGPL2.txt /out/licenses/libfuse.LGPL2.txt
cp /out/work/squashfuse/LICENSE /out/licenses/squashfuse.LICENSE

cd /out/work/fuse
patch --fuzz=0 -p1 < /sources/mount.c.diff
meson setup build --prefix=/usr --libdir=lib --default-library=static \
  -Dutils=false -Dexamples=false -Dtests=false -Duseroot=false .
ninja -j "$JOBS" -C build install

cd /out/work/squashfuse
export CFLAGS='-ffunction-sections -fdata-sections -Os -ffile-prefix-map=/out/work=/usr/src/type2-runtime'
./autogen.sh
./configure --disable-shared --enable-static --disable-demo LDFLAGS=-static
make -j "$JOBS"
make install
mkdir -p /usr/local/include/squashfuse
install -m 644 ./*.h /usr/local/include/squashfuse/

# Upstream tarball's version marker is UNSUPPORTED_LOCAL_DEVELOPER_BUILD;
# upstream CI substitutes the exact source commit before invoking make.
cd /out/work/runtime/src/runtime
printf '%s\n' "$RUNTIME_COMMIT" > version
make CC=clang CFLAGS="-std=gnu99 -Os -D_FILE_OFFSET_BITS=64 -DGIT_COMMIT=\\\"$RUNTIME_COMMIT\\\" -T data_sections.ld -ffunction-sections -fdata-sections -ffile-prefix-map=/out/work=/usr/src/type2-runtime -Wl,--gc-sections -Wl,-Map=/out/link.map -static -Wall -Werror -static-pie" runtime
strip --strip-debug --strip-unneeded runtime
# Type-2 magic must be patched after strip; see upstream build-runtime.sh.
printf 'AI\002' | dd of=runtime bs=1 count=3 seek=8 conv=notrunc
install -m 755 runtime /out/runtime-x86_64
/out/runtime-x86_64 --appimage-version > /out/runtime-version.txt 2>&1

# Tar archives carry original uid/gid; rootless Podman maps some extracted
# directories to subordinate host IDs. Remove work inside the same user
# namespace, before the host inspects output or writes a success manifest.
cd /
rm -rf /out/work
test ! -e /out/work
