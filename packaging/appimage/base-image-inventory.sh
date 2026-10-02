#!/bin/sh
# Run only in pinned, unmodified Ubuntu OCI image before APT replay.
set -eu
dpkg-query -W -f='${Package}\t${Version}\t${Architecture}\n' | sed 's/^/PACKAGE\t/'
find /usr/lib /lib64 /usr/share/doc \( -name '*.so*' -o -name copyright \) \( -type f -o -type l \) -print0 2>/dev/null |
  sort -zu | xargs -0 -r sha256sum | sed 's/^/FILE\t/'
