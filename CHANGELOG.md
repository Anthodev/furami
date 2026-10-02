# Changelog

## [Unreleased]

### Features

- Package the current generated-video prototype as a Linux x86_64 AppImage with isolated bundled Qt/QML and media libraries, host graphics drivers, checked build inputs, an actual-image linkage manifest and source/relink material inventory.
- Add manual `workflow_dispatch` AppImage build workflow entry that uploads an Actions artifact only and never publishes, including when dispatched on a tag ref.
- Add tag-driven AppImage release workflow: stable `vX.Y.Z` tags trigger tag/commit/Cargo-version validation, changelog note extraction, a two-build reproducibility comparison from the frozen media/runtime/app recipes, and a release publication gated on `release-ready.json` source and license clearance.
- Add read-only Linux UVC discovery with physical USB identities, native V4L2 descriptors, exact rational capture-mode validation and a JSON qualification probe (FUR-006).
- Open a command-line-selected UVC capture mode through the shared libmpv owner: explicit `--capture-node`, `--capture-fourcc`, `--capture-size` and `--capture-rate` selection, per-open identity and route revalidation off the GUI thread, requested-versus-observed verification with provenance (captured FourCC unverified, nominal-rate match approximate), typed stage-labeled errors with no retry or alternate mode, and close/reopen lifecycle on the existing panel buttons (FUR-007).

### Fixes

- Report missing or unreachable X11 displays with actionable XWayland/session guidance before Qt GUI construction.
- Stage an executable AppRun and reject non-executable launch entries in the final-image audit.
- Parse empty Deb822 checksum headers correctly and attribute built-in QML generated metadata through verified vendor backing modules.
- Isolate CI Rust toolchain state in the job's temporary directory to avoid component conflicts with the runner's preinstalled or partial toolchains.

### Maintenance

- Limit the internal AppImage inventory to extracted-tree dependency and static-source checks; remove the historical standalone diagnostic CLI without changing final-image audit results.

### Qualification

- The NV12 2560×1440 at 60/1 capture request reached ready on the ShadowCast 3 (Fedora 44, AMD RX 7900 XTX): decoded 2560×1440 nv12 with a nominal 60 fps report, and the live console feed rendered in the embedded window. The captured FourCC stays unverified and the rate match approximate, per the documented verification limits. An unsupported YUYV 2560×1440 at 60/1 tuple was rejected at prevalidation without building the backend.
- Mounted, temporary desktop-entry and extracted playback were exercised on Fedora 44/KDE Wayland with an AMD RX 7900 XTX. Arch/Hyprland and Ubuntu/GNOME remain researched but untested.
- Public release clearance remains blocked by missing checked corresponding-source inputs and unresolved component terms. No release or remote CI execution is claimed.
