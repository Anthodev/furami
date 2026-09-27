<h1 align="center">Furami</h1>

<p align="center"><strong>Linux capture viewer, built with Rust and Qt.</strong><br>
The current bootstrap opens an empty QML window; capture and playback are not implemented yet.</p>

<p align="center">
  <a href="https://github.com/Anthodev/furami/actions/workflows/ci.yml"><img src="https://github.com/Anthodev/furami/actions/workflows/ci.yml/badge.svg?branch=develop" alt="Linux CI"></a>
  <img src="https://img.shields.io/badge/platform-Linux%20x86__64-informational" alt="Linux x86-64">
  <a href="LICENSE"><img src="https://img.shields.io/badge/source%20license-MIT-blue" alt="Furami-authored source license: MIT"></a>
</p>

> [!NOTE]
> Furami is at the application-foundation stage. The window contains no capture, video, audio, settings, or profiles yet.

## Build from source

Requirements: Rust 1.97.1, a C++ compiler, LLVM `lld` (or another CXX-Qt-supported linker), and the pinned Qt 6.11.2 kit with Qt Declarative and Quick Controls. Set `QMAKE` to that kit's `qmake` if multiple installations are available. The binary needs a graphical Linux session to run.

```sh
QT_HOME=/path/to/qt
export QMAKE="$QT_HOME/bin/qmake"
export LD_LIBRARY_PATH="$QT_HOME/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
./target/debug/furami
```

`RUST_LOG` controls structured startup logging; unset or empty means `info`. An invalid filter, missing display, or failed QML load exits with an error. The QML window is embedded in the executable, so it can be launched outside the repository.

For a qualified media build, first follow the [pinned media recipe](packaging/media/README.md), then set `FURAMI_MEDIA_PREFIX` to its `prefix/` and `QMAKE` to that prefix's `bin/qmake6` before running Cargo. The build checks the Qt version and, when the media prefix is supplied, libmpv's client API against the stack lock. The current bootstrap does not yet play video or retain a libmpv runtime dependency; embedding libmpv belongs to FUR-003.

Linux CI uses an Avrea Ubuntu 24.04 x86_64 runner, Rust 1.97.1, and the exact Qt version in `packaging/media/stack.lock.json`. CI installs an official prebuilt Qt kit for compilation; the separately qualified product stack combines media built from source with checksum-locked official Qt artifacts.

## License

Furami-authored source is [MIT](LICENSE). The pinned media build includes GPL mpv/FFmpeg and LGPL libplacebo/Qt; a combined application executable or AppImage cannot be described as MIT-only. The [FUR-002 core license audit](packaging/media/LICENSE-AUDIT.md) records actual build flags, source pins, static-link caveats and distribution obligations. Release AppImage inventory, matching notices and source access remain a FUR-005 gate after FUR-003; no prototype AppImage has legal release clearance.