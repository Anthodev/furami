# Changelog

## [1.0.0-rc.1] - 2026-10-18

First release candidate for `1.0.0`, prepared for local evaluation. Furami is not stable and this candidate carries no certification: no tag, release or remote CI run has been published for it, and the qualified AppImage remains `0.1.0-rc.4` because no artifact was rebuilt for this version. Publication stays gated on the source and license clearance for every public artifact, stable or pre-release.

### Fixes

- Prevent compositor borders and shadows around the three transparent control windows under XWayland, and explicitly activate clicked or touched controls so subsequent keyboard input reaches them ([#25](https://github.com/Anthodev/furami/issues/25), uncommitted local work).

## [0.1.0-rc.4] - 2026-10-17

Fourth release candidate, prepared for local evaluation. Furami is not stable and this candidate carries no certification. Publication stays gated on the source and license clearance for every public artifact, stable or pre-release. No production behavior changes in this candidate.

### Fixes

- Keep the driver channel alive through cleanup in the captured-renderer regression test so the asynchronous owner can no longer fail sending its terminal message, removing an intermittent `SendError` in CI and leaving strict sender assertions, product code and workflow unchanged ([#25](https://github.com/Anthodev/furami/issues/25), uncommitted local work).

## [0.1.0-rc.3] - 2026-10-17

Third release candidate, prepared for local evaluation. Furami is not stable and this candidate carries no certification. Publication stays gated on the source and license clearance for every public artifact, stable or pre-release. The clean-checkout AppImage build repair is prepared locally; a successful fresh CI build remains unverified.

### Fixes

- Build the AppImage application from a private writable copy of the verified source snapshot instead of the read-only checkout, so CXX-Qt can generate its QML language-server configuration on clean CI checkouts without importing ignored developer files or touching the checkout ([#25](https://github.com/Anthodev/furami/issues/25), uncommitted local work).

## [0.1.0-rc.2] - 2026-10-17

Second release candidate, prepared for local evaluation. Furami is not stable and this candidate carries no certification: no tag, release or remote CI run has been published, and publication stays gated on the source and license clearance for every public artifact, stable or pre-release. The primary AppImage build failure behind this candidate is still unresolved.

### Fixes

- A failed AppImage CI build now preserves its diagnostic logs instead of aborting evidence collection ([#25](https://github.com/Anthodev/furami/issues/25), uncommitted local work).

## [0.1.0-rc.1] - 2026-10-17

First release candidate, prepared for local evaluation. Furami is not stable: this candidate carries no certification, and no tag, release or remote CI run has been published. Publication stays gated on the `release-ready.json` source and license clearance for every public artifact, stable or pre-release. The entries below combine the feature work merged into `develop` with the release-candidate chain that is still uncommitted local work of [#25](https://github.com/Anthodev/furami/issues/25).

### Features

- Add read-only UVC discovery with physical USB identities and exact rational capture-mode validation, and play the selected stream through the shared libmpv owner with requested-versus-observed verification and typed errors that never retry or fall back to another mode ([#34](https://github.com/Anthodev/furami/pull/34), [#35](https://github.com/Anthodev/furami/pull/35)).
- Capture console audio through one Furami-owned PipeWire loopback: explicit source selection without fallback, capture enablement kept separate from playback volume and mute, and **Auto** or pinned **Manual** output routing that goes silent when its sink disappears ([#37](https://github.com/Anthodev/furami/pull/37), [#42](https://github.com/Anthodev/furami/pull/42)).
- Select everything in the window and apply it immediately, with no separate Apply step: real source, resolution, exact Framerate, input-format policy (Auto ranks `YUYV`, then `NV12`, then `MJPG`, with manual overrides kept) and optional capture audio ([#38](https://github.com/Anthodev/furami/pull/38), [#47](https://github.com/Anthodev/furami/pull/47), [#48](https://github.com/Anthodev/furami/pull/48)).
- Replace the adjacent control panel with the soft Brume ribbon: hover volume/mute, contextual Options, Catppuccin Macchiato-inspired default palette, stream statistics at the bottom left, and controls that hide when idle and while another application has focus ([#48](https://github.com/Anthodev/furami/pull/48)).
- Add playback controls: a confirmed pause that freezes the picture and silences output, a resume that shows the console's current scene without replaying the pause, immediate volume and mute on Furami's own output, and fullscreen with previous geometry and focus restored ([#39](https://github.com/Anthodev/furami/pull/39)).
- Remember an automatic profile per capture source — resolution, exact rational Framerate, input-format policy, renderer settings and capture input audio — while output routing, volume, mute, fullscreen, statistics visibility and diagnostic logging stay global ([#41](https://github.com/Anthodev/furami/pull/41), [#48](https://github.com/Anthodev/furami/pull/48)).
- Add typed SDR renderer settings applied as one whole-treatment transaction with correlated read-back and a single restoration on failure ([#45](https://github.com/Anthodev/furami/pull/45)).
- Recover capture after device loss from the last successfully applied selection, keeping video running silently when only audio is lost and asking for your choice on ambiguous matches ([#40](https://github.com/Anthodev/furami/pull/40)).
- Add bounded diagnostic logs and a sanitized environment/session report, off by default and enabled only by explicit opt-in ([#50](https://github.com/Anthodev/furami/pull/50)).
- Enforce keyboard-shortcut precedence — `Space`, `F11` and `Esc` stay safe in text entry and popups — and add five-point wheel volume over the video or the volume rail ([#51](https://github.com/Anthodev/furami/pull/51)).
- feat(packaging): build and audit portable AppImage artifacts ([326ad30](https://github.com/Anthodev/furami/commit/326ad30)) — the image carries bundled Qt/QML and media libraries, checked build inputs, an actual-image linkage manifest and a source/relink inventory.
- build(packaging): version frozen AppImage runtime recipes ([44605aa](https://github.com/Anthodev/furami/commit/44605aa)).
- ci(packaging): add manual and tag-driven AppImage workflows ([75905ec](https://github.com/Anthodev/furami/commit/75905ec)) — a manual dispatch uploads an unpublished artifact only.
- Show the Furami mark as the application icon: the window icon is embedded in the binary and every window inherits it, and the same mark ships as the packaged scalable desktop icon with its `.DirIcon` (uncommitted local work of [#25](https://github.com/Anthodev/furami/issues/25)).
- Prepare the stable and release-candidate publication chain for [#25](https://github.com/Anthodev/furami/issues/25) (uncommitted local work): the application version is `0.1.0-rc.1`, the release-note extractor and tag workflow accept `vX.Y.Z` stable and `vX.Y.Z-rc.N` pre-release tags while requiring the exact tag/version match, the published asset is renamed to `Furami-v<version>-Linux-x64.AppImage`, stable tags are marked GitHub **Latest** and pre-releases are not, the pinned Ubuntu terms lock and the Qt module-terms rule feed the source and license guard, the two-build comparator decides reproducibility from its accepted-criteria predicate, and a dedicated `--qualification-video` route lets the packaged smoke run without a capture card. The pinned Alpine runtime inputs move to an authenticated refresh: the public candidate materials carry the corresponding runtime/Furami/Qt sources and the linked Alpine source closure, while the Alpine binary bootstrap packages and base image are excluded and retained only in a private authenticated cache — those public assets are therefore **not** a complete offline bootstrap, and the excluded tooling is neither distributed nor declared license-cleared. No tag, release, remote CI run or certification has occurred, and the rename adds no embedded AppImage update information or `.zsync`.

### Fixes

- Keep `--capture-audio-off` and `--capture-audio-source` session-only when startup adapts an incompatible video mode, preserve the admitted physical node/USB binding across Restart, Resume, Recovery and Reconnect, and reject malformed Auto settings whose concrete FourCC is outside the ranked formats ([#47](https://github.com/Anthodev/furami/pull/47), [#48](https://github.com/Anthodev/furami/pull/48)).
- Bind the permanent native root before showing its chrome windows, keep choice rows clear of their radio indicators and long ribbon captions elided, and show frame rates as rounded human labels while the exact rational stays the selected value ([#48](https://github.com/Anthodev/furami/pull/48)).
- Require fresh decoded-video readiness within a 10-second bound instead of treating ordinary pre-proof native log errors as lost renderer evidence ([#50](https://github.com/Anthodev/furami/pull/50)).
- Ship the verified provider `mpv` inside the existing recursive ELF closure and relocation audit ([#50](https://github.com/Anthodev/furami/pull/50)).
- Open the diagnostics log folder through an isolated child `xdg-open` and report launcher failures after startup without blocking the window ([#50](https://github.com/Anthodev/furami/pull/50)).
- Stage an executable AppRun and reject non-executable launch entries in the final-image audit, parse empty Deb822 checksum headers correctly, and attribute built-in QML generated metadata through verified vendor backing modules ([326ad30](https://github.com/Anthodev/furami/commit/326ad30), [75905ec](https://github.com/Anthodev/furami/commit/75905ec)).
- fix(startup): report unavailable X11 displays before Qt initialization ([d0754ed](https://github.com/Anthodev/furami/commit/d0754ed)).
- Keep CI and its tests honest: select the GNU BFD linker for the pinned toolchain and wait for real worker retirement in the owner-disconnection regression tests ([#42](https://github.com/Anthodev/furami/pull/42)), run `cargo fmt --all -- --check` as the first Cargo step ([#43](https://github.com/Anthodev/furami/pull/43)), and fix(ci): isolate Rust toolchain state per job ([24fb784](https://github.com/Anthodev/furami/commit/24fb784)).

### Removed

- Remove the product video-filter system: catalog discovery, chain editor and parameters, filter commands, VF application and proofs, and active/requested filter diagnostics. Renderer settings and their verified transactions remain ([#50](https://github.com/Anthodev/furami/pull/50)).
- Migrate saved settings lazily from format 1 and 2 to strict format 3: every other profile and preference loads unchanged, only the retired filter chains are ignored, the file is rewritten only at the next verified apply or safe close, and older builds refuse format 3 — there is no automatic downgrade and no way to recover removed chains by switching binaries ([#50](https://github.com/Anthodev/furami/pull/50)).
