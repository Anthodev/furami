#!/bin/sh
# Authenticate or install exactly the producer's private locked APK snapshot.
set -eu
: "${BASE_APK_DB_SHA256:?missing immutable base database digest}"
mode=install
if [ "${1:-}" = "--index" ]; then
    mode=index
    shift
fi
test "$#" -gt 0 || { echo 'selected APK list is empty' >&2; exit 1; }
exec 3</selected-apks/SHA256SUMS
for archive do
    case "$archive" in
        /selected-apks/*/*.apk) exit 1 ;;
        /selected-apks/*.apk) ;;
        *) exit 1 ;;
    esac
    test -f "$archive" && test ! -L "$archive" || exit 1
    IFS= read -r entry <&3
    checksum=${entry%% *}
    test "$entry" = "$checksum  $archive" || { echo 'selected APK list differs from checksums' >&2; exit 1; }
    test "${#checksum}" -eq 64
    case "$checksum" in *[!0-9a-f]*) exit 1 ;; esac
done
extra=
if IFS= read -r extra <&3 || [ -n "$extra" ]; then
    echo 'selected APK list differs from checksums' >&2
    exit 1
fi
exec 3<&-
echo "$BASE_APK_DB_SHA256  /lib/apk/db/installed" | sha256sum -c -
sha256sum -c /selected-apks/SHA256SUMS
# Every dependency comes from the immutable base or the explicit archive list.
apk --no-network --repositories-file /dev/null verify "$@"
if [ "$mode" = index ]; then
    apk --no-network --repositories-file /dev/null index --output /proof/selected-APKINDEX.tar.gz "$@"
else
    apk --no-network --repositories-file /dev/null add --no-cache "$@"
fi
