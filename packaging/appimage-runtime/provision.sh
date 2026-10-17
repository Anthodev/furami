#!/bin/sh
# Install only the producer's SHA256-checked selected archive arguments.
set -eu
: "${BASE_APK_DB_SHA256:?missing immutable base database digest}"
test "$#" -gt 0 || { echo 'selected APK list is empty' >&2; exit 1; }
for archive in "$@"; do
    case "$archive" in
        /sources/apks/*.apk) test -f "$archive" && test ! -L "$archive" ;;
        *) echo 'APK argument is outside the selected input directory' >&2; exit 1 ;;
    esac
done
echo "$BASE_APK_DB_SHA256  /lib/apk/db/installed" | sha256sum -c -
apk --no-network --repositories-file /sources/indexes/repositories verify "$@"
apk --no-network --repositories-file /sources/indexes/repositories update
apk --no-network --repositories-file /sources/indexes/repositories add --no-cache "$@"
