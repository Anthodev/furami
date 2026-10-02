#!/usr/bin/bash
# Build-only bind over /out/prefix/libexec/rcc. Original provider stays read-only.
# qt-build-utils 0.10.0 clears subprocess environment, so embed the locked epoch.
set -euo pipefail
export SOURCE_DATE_EPOCH=@SOURCE_DATE_EPOCH@
export QT_RCC_SOURCE_DATE_OVERRIDE=@SOURCE_DATE_EPOCH@
printf -v line '%q ' "$@"
printf 'SOURCE_DATE_EPOCH=%s QT_RCC_SOURCE_DATE_OVERRIDE=%s %s %s\n' \
  "$SOURCE_DATE_EPOCH" "$QT_RCC_SOURCE_DATE_OVERRIDE" \
  /frozen-provider/prefix/libexec/rcc "$line" >> /build/evidence/rcc-commands.txt
exec /frozen-provider/prefix/libexec/rcc "$@"
