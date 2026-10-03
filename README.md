<h1 align="center">Furami</h1>

<p align="center"><strong>Single-window video playback with an adjacent control panel.</strong><br>
Linux capture viewer prototype, built with Rust and Qt.</p>

<p align="center">
  <a href="https://github.com/Anthodev/furami/actions/workflows/ci.yml"><img src="https://github.com/Anthodev/furami/actions/workflows/ci.yml/badge.svg?branch=develop" alt="Linux CI"></a>
  <img src="https://img.shields.io/badge/platform-Linux%20x86__64-informational" alt="Linux x86-64">
  <a href="LICENSE"><img src="https://img.shields.io/badge/source%20license-MIT-blue" alt="Furami-authored source license: MIT"></a>
</p>

> [!IMPORTANT]
> Furami is an **early prototype**. Its purpose is low-latency viewing of a game console captured on the same machine. Playback of a selected UVC capture mode is available through explicit command-line selection; there is no device picker. Audio capture runs only through an explicitly named PulseAudio/PipeWire source, and audio loss is never recovered automatically: it is reported and requires a full capture restart. It has been exercised on Fedora 44 with KDE Plasma (Wayland/XWayland, AMD RX 7900 XTX), with partial desktop qualification and no broad hardware compatibility claims. Arch and Ubuntu sessions are expected to work under the same prerequisites but were not tested. An unexpected Qt-initiated video-surface loss enters a terminal failure state and permits controlled closure; restarting the application is required. Already-destroyed X11 surfaces and X server loss are not covered.

## What it does

Furami is a Linux project for viewing console capture in one window, with playback controls in an adjacent collapsible panel.

The current prototype provides:

- **One playback window.** Video and controls share one application window.
- **Capture playback.** Start the UVC capture mode selected on the command line, from the panel.
- **Named audio source capture.** Capture and play the audio of one explicitly named Pulse source alongside the video.
- **Capture restart.** Restart the whole capture, video and audio together, from the panel; there is no automatic recovery.
- **Collapsible control panel.** Close the session, toggle the panel and fullscreen, and view playback state with the audio status.
- **Fullscreen playback.** Toggle fullscreen from the panel or keyboard.
- **Keyboard and pointer control.** Playback shortcuts apply when the video has focus; text fields are intended to keep priority while editing.

## Selecting a capture mode

Capture is opt-in on the command line; all four options are required together:

```sh
./furami --capture-node /dev/video0 --capture-fourcc NV12 --capture-size 2560x1440 --capture-rate 60/1
```

Without a selection the window opens idle with capture disabled and an instruction to select a mode on the command line. At startup Furami parses the complete checked tuple and resolves the device identity. Each open validates the requested tuple against the device's advertised modes on a fresh route, off the GUI thread, before the media backend is built. An out-of-route tuple is rejected before the backend starts.

What Furami reports is separated into requested facts (what you asked for) and observed facts (what the backend reports, with provenance). The captured FourCC itself stays unverified: mpv exposes the decoded pixel format, which never proves the format the device delivered. The observed frame rate comes from the backend's nominal `container-fps` property, which can be wrong, so a match is labeled approximate rather than certified; a clear mismatch (for example 30 reported against 60 requested) fails the open.

## Audio capture

Audio is opt-in per open. `--list-audio-sources` prints the sources Pulse currently advertises, one `name<TAB>description` per line, and exits:

```sh
./furami --list-audio-sources
./furami --capture-node /dev/video0 --capture-fourcc NV12 --capture-size 2560x1440 --capture-rate 60/1 \
  --capture-audio-source alsa_input.usb-GENKI_ShadowCast_3_KT044001-02.analog-stereo
```

Rules that hold from one open to the next:

| Rule | Behavior |
| --- | --- |
| Explicit source only | The named source must exist, be unique, and stay compatible (same serial-level identity) at every fresh open. There is no default-source fallback; `@DEFAULT_SOURCE@` and aliases are rejected at argument parsing. |
| Disabled retains the source | `--capture-audio-off`, or disabling audio later, keeps the exact selected source attached to the session. Re-enabling never picks a microphone by default; without a retained source, enabling is rejected with a `SelectionRequired` diagnostic. |
| Audio changes reopen | Enabling or disabling audio is not a live toggle. It closes the current session and performs a complete new open of both streams under a new generation. |
| Loss is reported, never healed | If the audio transport fails (source removal, transport read error), the session stays open and the panel shows `RestartRequired` with the diagnostic. Continued video playback is not guaranteed. The only way to get audio back is a full capture restart. |

Audio and video are two different things to control. Playback volume and mute change only Furami's own playback output; they never touch the capture device's mixer or any other application. Disabling captured audio stops recording the source entirely and is not a mute. The panel displays the current audio status (`Disabled`, `Opening`, `Active`, `RestartRequired`) and the retained source; volume and mute are currently exposed only through the qualification stdin, not as panel widgets.

## Restart, not recovery

A capture restart, from the panel's **Restart capture** button or the qualification stdin, always tears down the whole media stack and rebuilds it: the current owner is stopped, its audio recording connection is quiesced, the stop is acknowledged, the old native parent is released, and only then does a new generation open video and audio together. There is no path that re-adds audio to a running session or switches source on the fly.

## Qualification stdin

`--qualification-stdin` reads one command per line from standard input for scripted hardware qualification. Every command except `open` requires the current generation number; a stale generation is rejected:

| Command | Effect |
| --- | --- |
| `open 0` | Open capture when idle and no attempt was made yet (`0` is the initial expected last generation; only `open` accepts it) |
| `open N` | Reopen capture from idle after a completed close, expecting last generation `N` |
| `restart N` | Full capture restart of the running, opening, or failed session last attempted at generation `N` |
| `enable N` / `disable N` | Enable or disable captured audio; both perform a complete reopen and keep the retained source |
| `volume N V` | Set playback volume to `V` percent, `0` to `100` |
| `mute N on` / `mute N off` | Set playback mute only; never the capture device |
| `close N` | Close the session and return to idle |
| `quit N` | Quit the application through the owner-acknowledged shutdown path |

Unknown commands, extra fields, oversized lines (over 256 bytes) and stale generations are rejected and logged, never guessed.

## Trying it

1. Launch an [AppImage](#appimage) or the [source build](#build-from-source) with a [capture selection](#selecting-a-capture-mode). The window opens in the `Idle` state.
2. Click **Open capture** in the panel. The video starts playing.
3. Use `Space`, `F11`, and the panel buttons while it plays.
4. When the stream ends, the session stays open. Click **Close session**, then **Open capture** to restart.
5. Click **Close session** to stop and return to idle, or quit the application normally.

## AppImage

The [AppImage recipe](packaging/appimage/README.md) produces one portable Linux x86_64 artifact with bundled Qt/QML and the frozen media libraries. A published release is not available from this change, and the package's source/license completeness gate is not yet cleared.

From the directory containing a built artifact:

```sh
sha256sum --check SHA256SUMS
chmod +x Furami-0.1.0-x86_64.AppImage
./Furami-0.1.0-x86_64.AppImage
```

No `FURAMI_MEDIA_PREFIX` is needed for an AppImage. On Wayland, the host must provide XWayland and a usable `DISPLAY`; Vulkan and graphics drivers also come from the host. See [portability requirements and tested status](packaging/appimage/PORTABILITY.md). File-manager double-click behavior depends on the desktop's executable-file association; menu integration is optional.

To run without mounting the image:

```sh
./Furami-0.1.0-x86_64.AppImage --appimage-extract
./squashfs-root/AppRun
```

Mounted, desktop-entry and extracted playback were exercised on Fedora 44/KDE Plasma Wayland with an AMD RX 7900 XTX. The final qualification run also played named-source console audio inside the mounted image; the user reported it audible and synchronized (latency perceived as poor and out of scope). Extraction was tested on this FUSE-capable host, not on a separate FUSE-less system. Arch/Hyprland and Ubuntu/GNOME compatibility is researched but untested.

## Controls

| Input | Action |
| --- | --- |
| `Space` | Pause or resume playback when the video has focus |
| `F11` | Toggle fullscreen |
| `Esc` | Leave fullscreen, otherwise hide the control panel |
| `Tab` / `Shift+Tab` | Move focus between controls |
| **Open capture** | Start the selected capture mode (disabled without a command-line selection) |
| **Restart capture** | Stop and fully reopen the capture, video and audio, under a new generation |
| **Close session** | Stop playback and return to idle |
| **Fullscreen / Leave fullscreen** | Toggle fullscreen from the panel |

Text typed in a panel field is intended to take precedence over playback shortcuts until focus leaves the field. The Fedora text-entry check did not qualify this behavior (see limitations).

## Not implemented yet

- **Device picker** — the capture mode and audio source are chosen only on the command line; there is no in-app device, mode or source browser.
- **Panel audio controls** — playback volume and mute exist but are exposed only through the [qualification stdin](#qualification-stdin); there is no slider or mute button in the panel, and no icon or warning design for `RestartRequired` yet.
- **Settings and profiles** — no persisted configuration and no per-game or per-console profiles.

## Known limitations

- Capture verification is bounded by what the media backend reports. The captured FourCC is never proven; the observed rate is a nominal property, so a reported match is approximate, not a certified cadence. The backend's request options are requests, not assertions, so an internal substitution by the pinned FFmpeg stack cannot be fully excluded; decisive contradictions (decoded size, or a clearly different nominal rate) fail the open.
- Advertised-but-refused modes fail with the requested tuple named at the stage the backend reports. Busy and permission failures are typed only when the backend evidence (operation and errno) is sufficient; otherwise the raw diagnostic is shown as-is. No alternate mode is attempted and there is no retry loop. After a failure the session cannot resume by itself; **Restart capture** performs a complete reopen under a new generation, or you can close the application.
- Audio qualification is functional, not metrological. The audio path was exercised natively and inside the AppImage bundle with a real GENKI ShadowCast 3 source on PipeWire/PulseAudio: source listing, exact-source opens, retained-source disable/enable cycles, full restarts after a paused source, and complete generation cycles with ordered owner acknowledgement and no owned audio nodes or mapped windows left after cleanup. Real Switch content (HDMI) rendered with sound in the final bundle; the user reported the audio audible and synchronized. Perceived latency was reported as poor and is explicitly out of scope. The captured FourCC and frame rate remain unverified as before, no latency number was measured, physical unplug of the selected source was not induced (only removal of Furami's own recording transport), and the five-minute listening request was not clocked as a measured gameplay duration. There is no automatic same-source recovery, and audio continuity across a source outage is not promised.
- An unexpected Qt-initiated video-surface loss while playing latches an explicit failure state and keeps the application open. The failed session cannot recover; controlled closure exits with a failure status and restarting the application is required. This does not cover an already-destroyed X11 surface or loss of the X server itself.
- Tested on Fedora 44 with KDE Plasma (Wayland/XWayland) and an AMD RX 7900 XTX (RADV Vulkan). Minimize/restore, desktop switching, fullscreen toggling and multi-monitor moves passed at 100%, 150% and 200% display scale; the panel popup passed at 100%. Qualification is partial: the input-test driver could not exercise pointer-dependent checks at 150% and 200%, Tab focus at 100% lacked visible confirmation, resizing stayed fixed under the desktop's tiling rules, and a text-entry check triggered pause/resume without populating the field. The cause of that text-entry failure is not isolated. Arch (Hyprland) and Ubuntu LTS (GNOME) sessions are untested.
- The prototype uses XWayland rather than native Wayland and requires hardware Vulkan rendering.

## Build from source

Requirements: Rust 1.97.1, a C++ compiler, LLVM `lld` or another CXX-Qt-supported linker, X11/XCB development packages including `xcb-shape`, the pinned Qt 6.11.2 kit with Qt Declarative and Quick Controls, and the runtime library `libpulse.so.0` (any PulseAudio/PipeWire installation provides it; only the runtime library is linked, no development headers or `.so` symlink are needed). To run the prototype, first build the media prefix using the [pinned media recipe](packaging/media/README.md). `FURAMI_MEDIA_PREFIX` must point to that prefix using an absolute path.

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
