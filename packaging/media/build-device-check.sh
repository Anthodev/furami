#!/usr/bin/env bash
# Validate actual ffmpeg -devices output: D means input support; E alone does not.
set -euo pipefail
if [[ $# != 1 || ! -f "$1" ]]; then
  echo 'usage: build-device-check.sh FFMPEG_DEVICES_TRANSCRIPT' >&2
  exit 2
fi
LC_ALL=C awk '
  /^[[:space:]]*D[ E][[:space:]]+/ {
    if ($1 != "D" && $1 != "DE") next
    count = split($2, aliases, ",")
    for (i = 1; i <= count; i++) {
      if (aliases[i] == "v4l2") video = 1
      if (aliases[i] == "pulse") audio = 1
    }
  }
  END {
    if (!video) print "FFmpeg input device missing: v4l2" > "/dev/stderr"
    if (!audio) print "FFmpeg input device missing: pulse" > "/dev/stderr"
    exit !video || !audio
  }
' "$1"
