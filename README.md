<h1 align="center">Furami</h1>

<p align="center"><strong>Linux capture viewer, built with Rust and Qt.</strong><br>
The current bootstrap opens an empty QML window; capture and playback are not implemented yet.</p>

<p align="center">
  <a href="https://github.com/Anthodev/furami/actions/workflows/ci.yml"><img src="https://github.com/Anthodev/furami/actions/workflows/ci.yml/badge.svg?branch=develop" alt="Linux CI"></a>
  <img src="https://img.shields.io/badge/platform-Linux%20x86__64-informational" alt="Linux x86-64">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT license"></a>
</p>

> [!NOTE]
> Furami is at the application-foundation stage. The window contains no capture, video, audio, settings, or profiles yet.

## Build from source

Requirements: Rust 1.97.1, a C++ compiler, and a Qt development kit at version 6.8 or newer with Qt Declarative and Quick Controls. Set `QMAKE` to the kit's `qmake` if multiple installations are available. The binary needs a graphical Linux session to run.

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

Linux CI uses an Avrea Ubuntu 24.04 x86_64 runner, Rust 1.97.1, and Qt 6.8.3.

## License

[MIT](LICENSE).