<h1 align="center">Furami</h1>

<p align="center"><strong>Single-window video playback with an adjacent control panel.</strong><br>
Linux capture viewer prototype, built with Rust and Qt.</p>

<p align="center">
  <a href="https://github.com/Anthodev/furami/actions/workflows/ci.yml"><img src="https://github.com/Anthodev/furami/actions/workflows/ci.yml/badge.svg?branch=develop" alt="Linux CI"></a>
  <img src="https://img.shields.io/badge/platform-Linux%20x86__64-informational" alt="Linux x86-64">
  <a href="LICENSE"><img src="https://img.shields.io/badge/source%20license-MIT-blue" alt="Furami-authored source license: MIT"></a>
</p>

> [!IMPORTANT]
> Furami is an **early prototype**. Its purpose is low-latency viewing of a game console captured on the same machine, but today it plays only a generated local test video; no capture device input exists yet. It was verified on one desktop (Wayland/XWayland with an AMD RX 7900 XTX) and makes no broad hardware compatibility claims. A known defect can close the application instead of recovering, so it is not yet reliable enough for daily use.

## What it does

Furami is a Linux project for viewing console capture in one window, with playback controls in an adjacent collapsible panel.

The current prototype provides:

- **One playback window.** Video and controls share one application window.
- **Collapsible control panel.** Open the test source, close the session, toggle the panel and fullscreen, and view playback state.
- **Fullscreen playback.** Toggle fullscreen from the panel or keyboard.
- **Keyboard and pointer control.** Playback shortcuts apply when the video has focus; text fields keep priority while editing.

## Trying it

1. Launch the application (see [Build from source](#build-from-source)). The window opens in the `Idle` state.
2. Click **Open proof source** in the panel. The video starts playing.
3. Use `Space`, `F11`, and the panel buttons while it plays.
4. When the video ends, the session stays open. Click **Close session**, then **Open proof source** to replay.
5. Click **Close session** to stop and return to idle, or quit the application normally.

## Controls

| Input | Action |
| --- | --- |
| `Space` | Pause or resume playback when the video has focus |
| `F11` | Toggle fullscreen |
| `Esc` | Leave fullscreen, otherwise hide the control panel |
| `Tab` / `Shift+Tab` | Move focus between controls |
| **Open proof source** | Start the generated test video |
| **Close session** | Stop playback and return to idle |
| **Fullscreen / Leave fullscreen** | Toggle fullscreen from the panel |

Text typed in a panel field takes precedence over playback shortcuts until focus leaves the field.

## Not implemented yet

- **Capture input** — no capture device, console, or network source; only the generated local test video plays.
- **Audio output** — playback is video-only today; no audio path is wired or claimed.
- **Settings and profiles** — no persisted configuration and no per-game or per-console profiles.

## Known limitations

- If the video display is lost while playing, the application can close instead of recovering. Restarting it is the only remedy.
- Verified only on Wayland/XWayland with an AMD RX 7900 XTX (RADV Vulkan). Other GPUs, compositors, and X11-only sessions are untested.
- The prototype uses XWayland rather than native Wayland and requires hardware Vulkan rendering.

## Build from source

Requirements: Rust 1.97.1, a C++ compiler, LLVM `lld` or another CXX-Qt-supported linker, X11/XCB development packages including `xcb-shape`, and the pinned Qt 6.11.2 kit with Qt Declarative and Quick Controls. To run the prototype, first build the media prefix using the [pinned media recipe](packaging/media/README.md). `FURAMI_MEDIA_PREFIX` must point to that prefix using an absolute path.

```sh
export FURAMI_MEDIA_PREFIX=/absolute/path/to/media/prefix
export RUSTC="$(rustup which rustc)"
export RUSTDOC="$(rustup which rustdoc)"
export QMAKE="$FURAMI_MEDIA_PREFIX/bin/qmake6"
export LD_LIBRARY_PATH="$FURAMI_MEDIA_PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export QT_PLUGIN_PATH="$FURAMI_MEDIA_PREFIX/plugins"
export QML_IMPORT_PATH="$FURAMI_MEDIA_PREFIX/qml"
cargo build --locked
./target/debug/furami
```

`RUST_LOG` controls diagnostic logging and defaults to `info`. With `FURAMI_MEDIA_PREFIX` unset, developer checks only need the Qt kit selected through `QMAKE` and `LD_LIBRARY_PATH`: `cargo test --locked` and `cargo clippy --locked --all-targets -- -D warnings`. They run without a display.

## License

Furami-authored source is [MIT](LICENSE). The pinned media build includes GPL mpv/FFmpeg and LGPL libplacebo/Qt, so a combined application executable cannot be described as MIT-only. The [core license audit](packaging/media/LICENSE-AUDIT.md) records actual build flags, source pins, static-link caveats and distribution obligations that apply to any redistribution.
