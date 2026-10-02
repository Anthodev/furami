# Changelog

## [Unreleased]

### Features

- Package the current generated-video prototype as a Linux x86_64 AppImage with isolated bundled Qt/QML and media libraries, host graphics drivers, checked build inputs, an actual-image linkage manifest and source/relink material inventory.
- Add manual `workflow_dispatch` AppImage build workflow entry that uploads an Actions artifact only and never publishes, including when dispatched on a tag ref.
- Add tag-driven AppImage release workflow: stable `vX.Y.Z` tags trigger tag/commit/Cargo-version validation, changelog note extraction, a two-build reproducibility comparison from the frozen media/runtime/app recipes, and a release publication gated on `release-ready.json` source and license clearance.

### Fixes

- Report missing or unreachable X11 displays with actionable XWayland/session guidance before Qt GUI construction.
- Stage an executable AppRun and reject non-executable launch entries in the final-image audit.
- Parse empty Deb822 checksum headers correctly and attribute built-in QML generated metadata through verified vendor backing modules.

### Maintenance

- Limit the internal AppImage inventory to extracted-tree dependency and static-source checks; remove the historical standalone diagnostic CLI without changing final-image audit results.

### Qualification

- Mounted, temporary desktop-entry and extracted playback were exercised on Fedora 44/KDE Wayland with an AMD RX 7900 XTX. Arch/Hyprland and Ubuntu/GNOME remain researched but untested.
- Public release clearance remains blocked by missing checked corresponding-source inputs and unresolved component terms. No release or remote CI execution is claimed.
