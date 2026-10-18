# Changelog

## [Unreleased]

### 🔄 Changes

**Interface**

- Show only the input format on the **Resolved input** statistics line, adding `(auto)` when that format was chosen automatically. Formats you select explicitly appear alone.

### 🐛 Fixes

**AppImage**

- Restore runtime builds that stopped with an `APKINDEX.tar.gz` checksum mismatch after the Alpine repository changed. Refresh the signed index and the pinned `libcurl` / `libexpat` build packages, while preserving signature, checksum and package-identity verification.

**Interface**

- Show input formats only once in Capture settings, using the existing dropdown instead of repeating its choices as radio buttons. Selection still applies immediately.

## [1.0.0-rc.2] - 2026-10-18

### 🔄 Changes

**AppImage**

- Apply the bundled FFmpeg patch automatically during a rebuild and verify it against a retained checksum; a mismatch rejects the build. This concerns only the privately bundled FFmpeg, never the FFmpeg installed on the host.
- Include the patch in the AppImage notices and corresponding source materials, and retain its application record in build provenance so future builds can verify and reconstruct the bundled changes.

### 🐛 Fixes

**Capture**

- Prioritize a recent video image instead of replaying older queued frames, addressing capture startup lag caused by accumulated frames. Each V4L2 read discards older completed frames within a bounded drain, without adding frame copies.
- Keep the packet buffer valid when the decoder still holds frames while capture shuts down, fixing a crash in the new freshness path.

Full changelog: [v1.0.0-rc.1...v1.0.0-rc.2](https://github.com/Anthodev/furami/compare/v1.0.0-rc.1...v1.0.0-rc.2)

## [1.0.0-rc.1] - 2026-10-18

First release candidate for `1.0.0`.

### 🔄 Changes

**Interface**

- Use English throughout the interface, including controls, settings, status and error messages, and accessibility descriptions. Keep the label **Framerate** and display fractional FPS with a decimal point.

### 🐛 Fixes

**Interface**

- Prevent compositor borders and shadows around the three transparent control windows under XWayland, and explicitly activate clicked or touched controls so subsequent keyboard input reaches them ([#25](https://github.com/Anthodev/furami/issues/25)).

Full changelog: [v0.1.0-rc.4...v1.0.0-rc.1](https://github.com/Anthodev/furami/compare/v0.1.0-rc.4...v1.0.0-rc.1)

## [0.1.0-rc.4] - 2026-10-17

### 🐛 Fixes

- Fix an intermittent `SendError` in the captured-renderer CI regression test by keeping its driver channel alive through cleanup. Product behavior is unchanged ([#25](https://github.com/Anthodev/furami/issues/25)).

## [0.1.0-rc.3] - 2026-10-17

### 🐛 Fixes

- Build the AppImage application from a private writable copy of the verified source snapshot instead of the read-only checkout, so CXX-Qt can generate its QML language-server configuration on clean CI checkouts without importing ignored developer files or touching the checkout. A successful fresh CI build was not established for this candidate ([#25](https://github.com/Anthodev/furami/issues/25)).

## [0.1.0-rc.2] - 2026-10-17

The primary AppImage build failure behind this candidate was still unresolved.

### 🐛 Fixes

- A failed AppImage CI build now preserves its diagnostic logs instead of aborting evidence collection ([#25](https://github.com/Anthodev/furami/issues/25)).

## [0.1.0-rc.1] - 2026-10-17

Entries combine the feature work merged into `develop` with the release-candidate chain tracked in [#25](https://github.com/Anthodev/furami/issues/25).

### ✨ New Features

**Capture and playback**

- Detect capture cards read-only over UVC, with their physical USB identities, and validate the exact capture mode you pick. The selected mode is checked against the observations that are actually available and contradictions are reported as typed errors, so nothing is silently retried or replaced by another mode; the captured FourCC and the measured cadence are not verified ([#34](https://github.com/Anthodev/furami/pull/34), [#35](https://github.com/Anthodev/furami/pull/35)).
- Pick source, resolution, exact frame rate, input format and optional capture audio in the window, with changes applied immediately and no Apply step. **Auto** ranks `YUYV`, then `NV12`, then `MJPG`; manual overrides are kept ([#38](https://github.com/Anthodev/furami/pull/38), [#47](https://github.com/Anthodev/furami/pull/47), [#48](https://github.com/Anthodev/furami/pull/48)).
- Pause with a confirmed freeze and silence, resume on the console's live scene, adjust volume and mute on Furami's own output, and go fullscreen with the previous geometry and focus restored ([#39](https://github.com/Anthodev/furami/pull/39)).
- Recover the stream from the last applied selection after a device loss, keep video running silently when only audio is lost, and ask when a device match is ambiguous ([#40](https://github.com/Anthodev/furami/pull/40)).

**Audio**

- Capture console audio through one Furami-owned PipeWire loopback: the input source is explicit with no fallback, capture is separate from playback volume and mute, and output routing is either **Auto**, which picks one eligible EasyEffects sink and otherwise the default output and re-resolves as that list changes, or a pinned **Manual** sink that is never replaced — if it disappears, the audio stays silent until you select another output ([#37](https://github.com/Anthodev/furami/pull/37), [#42](https://github.com/Anthodev/furami/pull/42)).

**Video**

- Apply SDR renderer settings as a single all-or-nothing change, verified by read-back and restored once if it fails ([#45](https://github.com/Anthodev/furami/pull/45)).

**Settings and profiles**

- Remember a profile per capture source — resolution, exact frame rate, input format, renderer settings and capture input audio — while output routing, volume, mute, fullscreen, statistics and diagnostic logging stay global ([#41](https://github.com/Anthodev/furami/pull/41), [#48](https://github.com/Anthodev/furami/pull/48)).

**Controls**

- Keep `Space`, `F11` and `Esc` safe in text entry and popups, and step the volume by five points with the wheel over the video or the volume rail ([#51](https://github.com/Anthodev/furami/pull/51)).

**Diagnostics**

- Record bounded diagnostic logs and a sanitized environment and session report, off by default and enabled only by explicit opt-in ([#50](https://github.com/Anthodev/furami/pull/50)).

**Application icon**

- Show the Furami mark as the application icon, embedded in the binary for every window and shipped as the packaged scalable desktop icon with its `.DirIcon` ([#25](https://github.com/Anthodev/furami/issues/25)).

### 🔄 Changes

**Interface**

- Replace the adjacent control panel with the soft Brume ribbon: hover volume and mute, contextual Options, a Catppuccin Macchiato-inspired palette, stream statistics at the bottom left, and controls that hide when idle or another application has focus ([#48](https://github.com/Anthodev/furami/pull/48)).

**Video filters**

- Remove the product video-filter system: catalog discovery, the chain editor and its parameters, filter commands, VF application and proofs, and active/requested filter diagnostics. Renderer settings and their verified changes remain ([#50](https://github.com/Anthodev/furami/pull/50)).
- Migrate saved settings lazily from format 1 and 2 to strict format 3: other profiles and preferences load unchanged, only the retired filter chains are dropped, and the file is rewritten only at the next verified apply or safe close. Older builds refuse format 3 — there is no automatic downgrade, and switching binaries cannot recover the removed chains ([#50](https://github.com/Anthodev/furami/pull/50)).

**Packaging and release**

- feat(packaging): build and audit portable AppImage artifacts ([326ad30](https://github.com/Anthodev/furami/commit/326ad30)) — the image bundles Qt/QML and media libraries, checked build inputs, an actual-image linkage manifest and a source/relink inventory.
- build(packaging): version frozen AppImage runtime recipes ([44605aa](https://github.com/Anthodev/furami/commit/44605aa)).
- ci(packaging): add manual and tag-driven AppImage workflows ([75905ec](https://github.com/Anthodev/furami/commit/75905ec)) — a manual dispatch uploads an unpublished artifact only.
- Prepare the stable and pre-release publication chain for [#25](https://github.com/Anthodev/furami/issues/25): the application version is `0.1.0-rc.1`; the tag workflow accepts `vX.Y.Z` stable and `vX.Y.Z-rc.N` pre-release tags and requires an exact tag/version match; the published asset is named `Furami-v<version>-Linux-x64.AppImage`; and stable tags are marked GitHub **Latest** while pre-releases are not.
- Gate publication on source and license clearance: the pinned Ubuntu terms lock and the Qt module-terms rule feed the source and license guard. The pinned Alpine runtime inputs move to an authenticated refresh — public candidate materials carry the runtime, Furami and Qt sources plus the linked Alpine source closure, while the Alpine binary bootstrap packages and base image stay in a private authenticated cache. Those public assets are therefore **not** a complete offline bootstrap, and the excluded tooling is neither distributed nor declared license-cleared.
- Decide reproducibility with the two-build comparator and its accepted-criteria predicate.
- Let the packaged smoke run without a capture card through a dedicated `--qualification-video` route.
- The asset rename adds no embedded AppImage update information and no `.zsync`.

### 🐛 Fixes

**Startup and capture**

- fix(startup): report unavailable X11 displays before Qt initialization ([d0754ed](https://github.com/Anthodev/furami/commit/d0754ed)).
- Keep `--capture-audio-off` and `--capture-audio-source` session-only when startup has to adapt an incompatible video mode, keep the admitted physical node and USB binding across Restart, Resume, Recovery and Reconnect, and reject malformed Auto settings whose concrete FourCC falls outside the ranked formats ([#47](https://github.com/Anthodev/furami/pull/47), [#48](https://github.com/Anthodev/furami/pull/48)).
- Bind the native root before showing its chrome windows, keep choice rows clear of their radio indicators and long ribbon captions elided, and show frame rates as rounded labels while the exact value stays selected ([#48](https://github.com/Anthodev/furami/pull/48)).

**Renderer and diagnostics**

- Require decoded video to become ready within 10 seconds instead of treating ordinary pre-proof log errors as lost renderer evidence ([#50](https://github.com/Anthodev/furami/pull/50)).
- Open the diagnostics log folder through an isolated `xdg-open` child and report launcher failures after startup without blocking the window ([#50](https://github.com/Anthodev/furami/pull/50)).

**Packaging**

- Ship the verified provider `mpv` inside the existing recursive ELF closure and relocation audit ([#50](https://github.com/Anthodev/furami/pull/50)).
- Stage an executable AppRun, reject non-executable launch entries in the final-image audit, parse empty Deb822 checksum headers, and attribute built-in QML generated metadata through verified vendor backing modules ([326ad30](https://github.com/Anthodev/furami/commit/326ad30), [75905ec](https://github.com/Anthodev/furami/commit/75905ec)).

**CI**

- Keep CI honest: select the GNU BFD linker for the pinned toolchain, wait for real worker retirement in the owner-disconnection regression tests ([#42](https://github.com/Anthodev/furami/pull/42)), and run `cargo fmt --all -- --check` as the first Cargo step ([#43](https://github.com/Anthodev/furami/pull/43)).
- fix(ci): isolate Rust toolchain state per job ([24fb784](https://github.com/Anthodev/furami/commit/24fb784)).
