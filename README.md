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
- **Console audio.** Routes the capture card's audio through one Furami-owned PipeWire loopback into your output. This requires a host PipeWire session providing the `pw-loopback` and `pw-metadata` helpers (see the [media recipe](packaging/media/README.md)); a standalone PulseAudio setup is not a supported transport. Audio stays optional — video-only viewing works without it.
- **Output choice.** **Auto** sends audio to EasyEffects' output sink when it is live and routable, otherwise to the current default output. **Manual** pins a sink of your choice and goes silent if it disappears — there is no fallback; when it returns, selecting it again is up to you.
- **Pause and resume.** `Space` freezes the picture and silences the output; resuming jumps to the console's current scene without replaying the pause.
- **Immediate volume and mute.** The panel's slider and mute button change Furami's playback output right away — never the capture device or system mixer.
- **Fullscreen.** `F11` toggles fullscreen and restores your previous window size and focus.
- **Collapsible controls.** Hide the panel when you want more space for the picture.
- **Capture recovery.** Reconnects the last applied selection when the same card returns; paused playback stays paused, and ambiguous matches require your choice.
- **Audio-only recovery.** Keeps video running silently while waiting for the selected audio source, without choosing a different input.
- **Saved settings.** Restores the last successfully applied capture and keeps the Auto/Manual output preference, local volume, mute and fullscreen settings across safe closes.

## First use

Choose your capture device and one of its supported video modes. All four options are required:

```sh
./furami --capture-node /dev/video0 --capture-fourcc NV12 --capture-size 2560x1440 --capture-rate 60/1
```

The values above are examples — use what your card actually supports. With no saved capture and no explicit selection, the window opens idle. Explicit capture arguments take precedence over automatic restoration until you apply them.

Audio is optional. It requires a running host PipeWire session with `pw-loopback` and `pw-metadata` installed at the usual locations; Furami never starts or installs a PipeWire daemon, session manager, or WirePlumber. List the available sources (through the PipeWire/PulseAudio catalog interface), then replace the example source name with your capture card's source:

```sh
./furami --list-audio-sources
./furami --capture-node /dev/video0 --capture-fourcc NV12 --capture-size 2560x1440 --capture-rate 60/1 \
  --capture-audio-source alsa_input.usb-MyCard-01.analog-stereo
```

## Using it

Click **Open capture** to apply an explicit selection. Normal launches automatically reopen the saved selection when its requested sources and exact mode are available; otherwise the panel shows the selection and the reason without starting a stream. The panel's output selector switches between **Auto** and **Manual** routing immediately, independently of the capture draft and **Apply**. Pause or resume with `Space` or the panel button, adjust volume and mute in the panel, and toggle fullscreen with `F11`. **Close session** returns to idle; **Restart capture** fully reopens the capture. **Reconnect** retries the last valid selection after a failure and leaves a healthy session unchanged.

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
| **Reconnect** button | Retry the last valid selection; join recovery already in progress |

Focused controls keep their keys: `Space` types a space or activates the focused control instead of pausing. No global shortcuts; fullscreen and focus stay local to the window.

## Saved settings

Furami stores a successfully applied capture selection plus the chosen **Auto/Manual output preference** and local volume, mute and fullscreen settings — saved at safe close even when idle or paused, and never a draft or live route. Closing with an edited, unapplied selection offers **Cancel close** or **Quit without applying**.

The settings format is strict schema version 1. The output preference is an optional `output` entry inside `preferences`; documents written before that entry existed (meaning Auto) still load. A **Manual** entry is refused by older strict builds, so return the output to **Auto** and safely close before downgrading — no destructive reset is involved, and the applied capture selection is retained either way.

The file is `$XDG_CONFIG_HOME/furami/settings.json` when `XDG_CONFIG_HOME` is an absolute path. If that variable is unset or empty, Furami uses `$HOME/.config/furami/settings.json`. An invalid location is reported rather than replaced with a guessed path.

A corrupt, invalid or newer-version file is preserved. All automatic saves remain blocked; preference changes are session-only until you explicitly confirm **Reset saved file**. Reset replaces the saved file with your current preferences and no saved capture. It does not stop live capture, apply the draft or reset live preferences.

## Limitations

- **New selections are command-line only.** Saved selections restore automatically, and recovery can ask you to choose between matching cards; there is no general device, mode or audio-source picker.
- **Profiles are not available.** Local application settings are saved, but named profiles have not been implemented.
- **Recovery qualification is pending.** Physical unplug/replug, recovery while paused, audio-only loss and post-recovery A/V still need live hardware verification.
- **Narrowly tested.** Fedora 44 / KDE Plasma Wayland with an AMD RX 7900 XTX is the exercised setup; Ubuntu and Arch are untested. See the [portability notes](packaging/appimage/PORTABILITY.md).
- **Hardware Vulkan is required.** Wayland sessions also need XWayland; software rendering is not supported.
- **Audio routing is a prototype pending extended qualification.** Basic picture, sound and capture controls are user-confirmed on the tested setup. Physical device-loss scenarios and sustained live A/V behaviour still need qualification.
- **Frame rate is playback configuration, not a measurement.** Furami derives mpv playback timing from the selected rate (for example `60/1` or `60000/1001`) and shows it as application-configured timing metadata. The card's real capture cadence is neither measured nor certified, and it may renegotiate internally.
- **Audio and video clocks are separate.** There is no common media clock; A/V synchronisation, drift and latency over sustained playback still need measurement.
- **Low latency is the goal, not a claim.** Furami promises no latency figure. The physical latency has not been measured, and long-session stability remains unqualified.

## Availability

Furami is not yet available as a release-ready download. The [AppImage recipe](packaging/appimage/README.md) builds development packages; the AppImage source/license completeness gate is not yet cleared.

## Building from source

For contributors, Furami uses Rust and Qt. The [media recipe](packaging/media/README.md) covers the pinned dependencies and prerequisites; the [AppImage documentation](packaging/appimage/README.md) covers application builds and packaging.

## License

Furami-authored source is [MIT](LICENSE). The pinned media build includes GPL mpv/FFmpeg and LGPL libplacebo/Qt, so a combined executable is not MIT-only; the [core license audit](packaging/media/LICENSE-AUDIT.md) records build flags and distribution obligations for redistribution.
