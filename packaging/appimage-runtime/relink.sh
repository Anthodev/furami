#!/bin/sh
# Rebuild runtime with an interface-compatible edited, unpatched FUSE 3.15.0
# source tarball. This script travels with relink/ and its pinned recipe files.
set -eu
if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    echo "Usage: relink.sh /absolute/new-output /absolute/modified-fuse-3.15.0.tar.xz [absolute-verified-cache]" >&2
    exit 2
fi
bundle=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
output=$1
modified=$2
cache=${3:-${XDG_CACHE_HOME:-$HOME/.cache}/furami/appimage-runtime}
case "$output:$modified:$cache" in
    /*:/*:/*) ;;
    *) echo 'all paths must be absolute' >&2; exit 2 ;;
esac
# Seed original source archives from the delivered bundle. build.py checks
# their locked SHA256 before invoking an offline container; APK/source cache
# inputs not bundled here must be supplied by recipient or fetched+verified.
mkdir -p "$cache"
for name in type2-runtime.tar.gz fuse-3.15.0.tar.xz squashfuse-0.5.2.tar.gz mount.c.diff; do
    if [ ! -e "$cache/$name" ]; then
        cp "$bundle/$name" "$cache/$name"
    fi
done
exec python3 "$bundle/build.py" "$output" --source-cache "$cache" --modified-libfuse "$modified"
