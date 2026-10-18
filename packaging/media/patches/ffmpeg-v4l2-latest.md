# Private FFmpeg V4L2 "latest frame" patch

`ffmpeg-v4l2-latest.patch` modifies FFmpeg's `libavdevice/v4l2.c` to prefer recent completed frames. The bundled recipe currently targets FFmpeg 8.1.2. Read this note before updating that recipe or porting the patch.

## What it is (and is not)

The patched V4L2 input module belongs to FFmpeg's `libavdevice`; it is not a separate dependency to update. The pristine FFmpeg archive and this separate patch reconstruct the modified source. [`../stack.lock.json`](../stack.lock.json) is authoritative for the upstream version/archive checksum and ordered `sources.ffmpeg.patches[]` entries, including this patch's path and SHA256.

The private `furami_latest` option is **off by default**. Furami enables it in the lavf capture input options, not the device URL (see [`../../../src/capture/input.rs`](../../../src/capture/input.rs)). Consumers that do not opt in retain the upstream path. Only the private bundled provider is modified, not system FFmpeg.

## How the recipe applies it

[`../build.py`](../build.py) verifies the patch and lock, snapshots both into the build inputs, and passes the lock hash into the container. The container recipe extracts fresh upstream FFmpeg source; [`../source-patches.py`](../source-patches.py) then applies the ordered patches with `git apply`: each patch is first `--check`ed and then applied with `--whitespace=error`. A hash mismatch, an invalid patch entry or a failed application stops the build. The applied-patch receipt (`source-patches.json`) and frozen inputs are re-verified after the build by `verify_provider`; a provider whose patches differ from the lock is rejected.

This means no manual `patch`/`git apply` step is part of the normal flow: keep the patch and its metadata consistent and the recipe reapplies it automatically on every provider build.

## Invariants to preserve on any rebase

The change separates mmap packet lifetime from capture-context lifetime. Rebasing on new upstream code must keep all of:

- **Independent owner lifetime.** A delivered packet can outlive the `AVFormatContext` (mpv's packet pool releases it late). The patch gives the mapped buffers their own reference-counted owner (`latest_owner`, `atomic_uint refs`); nothing reachable from a packet's release callback may point into `priv_data`.
- **Synchronous retirement.** Retire does `VIDIOC_STREAMOFF` then `close(fd)` under the owner mutex and clears `fd`, so no in-flight callback can use a closed/reused descriptor; the final owner reference `munmap`s the buffers.
- **Serialized DQ/QBUF/retire.** One mutex serializes `VIDIOC_DQBUF`, `VIDIOC_QBUF` and retirement, with queued-buffer accounting kept inside it.
- **Bounded drain.** After obtaining the first completed buffer, each read attempts at most `buffers - 1` additional dequeues, using the actual granted `REQBUFS` count. Waiting for the first buffer remains interruptible; continuous arrivals cannot make the subsequent drain unbounded.
- **Supported formats.** The enabled path supports single-planar NV12, YUYV, BGR3 and MJPEG; unsupported formats and multiplanar capture are rejected. The device is opened nonblocking.
- **Preserved failure handling.** Keep cancellation, DQ/QBUF errors, buffer accounting and allocation rollback correct. Discarded frames add no payload copies; retain upstream's low-buffer copy fallback.

## Update / rebase steps

1. **Fresh upstream source.** Take the new FFmpeg archive, not a reused provider tree.
2. **Rebase and preflight without compiling.** Against the unpacked new source, run the same check the recipe runs:
   ```sh
   git apply --check --whitespace=error /path/to/ffmpeg-v4l2-latest.patch
   ```
   Inspect the changed capture code even if this check passes. If needed, regenerate the patch against the new source and rerun the check; preserve every required behavior rather than dropping failed hunks or weakening application checks.
3. **Bump the metadata.** Update the `ffmpeg` `version`, `url`, `archive` and `sha256` in [`../stack.lock.json`](../stack.lock.json), and the patch `sha256` in `sources.ffmpeg.patches[]`. Refresh `media_lock_sha256` in [`../../appimage/app.lock.json`](../../appimage/app.lock.json) and [`../../appimage/tool-closure.lock.json`](../../appimage/tool-closure.lock.json) to the new stack-lock hash.
4. **Build a fresh provider.** Use the qualified media build flow in [`../README.md`](../README.md); do not reuse the previous provider. The build fails closed if the patched bytes, lock or receipt disagree.
5. **Rebuild the application** against the new prefix through the documented AppImage flow, and re-check the source receipt/material outputs that flow records.
6. **Regression on actual libraries.** With the new provider prefix and an empty output directory:
   ```sh
   python3 packaging/media/test-v4l2-latest.py --prefix /new/media/prefix --output /private/evidence/patched --mpv
   ```
   Run this probe from the repository root. `--baseline` omits the private option and runs stock first-frame assertions; `--mpv` adds actual libmpv null-VO integration. Then run the pinned Rust campaign and native AppImage smoke, and finish with physical startup/restart and audio on/off checks on real hardware.

## If upstream adopts an equivalent fix

Before removing the private option and patch, verify that the pinned upstream implementation preserves packet lifetime, safe retirement/requeue, and bounded freshness selection for the supported capture modes. Rerun behavioral checks and migrate Furami's input options together with the recipe; an upstream fix need not use the same internal structure.

## Proof boundary

The actual-library probes use a synthetic kernel adapter and do not open a physical V4L2 device, display or audio route. Native AppImage smoke separately exercises rendering, pause/resume and shutdown. The user reported successful tests on both the affected laptop and the local desktop. This is qualitative acceptance, not measured end-to-end latency or long-duration qualification.
