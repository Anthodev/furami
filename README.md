<h1 align="center">Furami</h1>

<p align="center"><strong>Watch and hear your game console on your Linux desktop.</strong><br>
View a console connected through a USB capture card, with playback controls beside the video.</p>

<p align="center">
  <a href="https://github.com/Anthodev/furami/actions/workflows/ci.yml"><img src="https://github.com/Anthodev/furami/actions/workflows/ci.yml/badge.svg?branch=develop" alt="Linux CI"></a>
  <img src="https://img.shields.io/badge/platform-Linux%20x86__64-informational" alt="Linux x86-64">
  <a href="LICENSE"><img src="https://img.shields.io/badge/source%20license-MIT-blue" alt="Furami-authored source license: MIT"></a>
</p>

> [!IMPORTANT]
> Furami is a development prototype. There is no installer; you select the capture device and mode on the command line, and only a small set of hardware is exercised. See [Limitations](#limitations).

## What it does

- **Live console viewing.** Plays the UVC capture mode you select, windowed or fullscreen.
- **Console audio.** Captures the audio of one PulseAudio/PipeWire source you name.
- **Pause and resume.** `Space` freezes the picture and silences the output; resuming jumps to the console's current scene without replaying the pause.
- **Immediate volume and mute.** The panel's slider and mute button change Furami's playback output right away — never the capture device or system mixer.
- **Fullscreen.** `F11` toggles fullscreen and restores your previous window size and focus.
- **Collapsible controls.** Hide the panel when you want more space for the picture.

## First use

Choose your capture device and one of its supported video modes. All four options are required:

```sh
./furami --capture-node /dev/video0 --capture-fourcc NV12 --capture-size 2560x1440 --capture-rate 60/1
```

The values above are examples — use what your card actually supports. Without a selection the window opens idle.

Audio is optional. List the available PulseAudio/PipeWire sources, then replace the example source name with your capture card's source:

```sh
./furami --list-audio-sources
./furami --capture-node /dev/video0 --capture-fourcc NV12 --capture-size 2560x1440 --capture-rate 60/1 \
  --capture-audio-source alsa_input.usb-MyCard-01.analog-stereo
```

## Using it

Click **Open capture** to start playback. Pause or resume with `Space` or the panel button, adjust volume and mute in the panel, toggle fullscreen with `F11`. **Close session** returns to idle; **Restart capture** fully reopens the capture; quitting closes everything cleanly.

Resuming briefly reconnects: the panel shows `Resuming` for a moment, then the console's current scene.

## Controls

| Input | Action |
| --- | --- |
| `Space` | Pause or resume, when the video or window has focus |
| `F11` | Toggle fullscreen for the Furami window only |
| `Esc` | Leave fullscreen, otherwise hide the control panel |
| `Tab` / `Shift+Tab` | Move focus between controls |
| **Pause / Resume** button | Same as `Space`, from the panel |
| **Volume slider** | Furami's playback volume, 0–100, immediate |
| **Mute / Unmute** button | Furami's playback output only |

Focused controls keep their keys: `Space` types a space or activates the focused control instead of pausing. No global shortcuts; fullscreen and focus stay local to the window.

## Limitations

- **Selection is command-line only.** No in-app device, mode or audio-source picker.
- **Settings are not persisted.** Every launch restates the capture selection; no profiles.
- **Audio loss needs a full restart.** A failed audio transport shows `RestartRequired`; a full capture restart is the only way back.
- **Narrowly tested.** Fedora 44 / KDE Plasma Wayland with an AMD RX 7900 XTX is the exercised setup; Ubuntu and Arch are untested. See the [portability notes](packaging/appimage/PORTABILITY.md).
- **Hardware Vulkan is required.** Wayland sessions also need XWayland; software rendering is not supported.
- **Low latency is the goal, not a claim.** Latency has not been measured or tuned.

## Availability

Furami is not yet available as a release-ready download. The [AppImage recipe](packaging/appimage/README.md) builds development packages; the AppImage source/license completeness gate is not yet cleared.

## Building from source

For contributors, Furami uses Rust and Qt. The [media recipe](packaging/media/README.md) covers the pinned dependencies and prerequisites; the [AppImage documentation](packaging/appimage/README.md) covers application builds and packaging.

## License

Furami-authored source is [MIT](LICENSE). The pinned media build includes GPL mpv/FFmpeg and LGPL libplacebo/Qt, so a combined executable is not MIT-only; the [core license audit](packaging/media/LICENSE-AUDIT.md) records build flags and distribution obligations for redistribution.
