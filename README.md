<h1 align="center">Furami</h1>

<p align="center"><strong>Watch and hear your game console on your Linux desktop.</strong><br>
View a console connected through a USB capture card, with playback controls beside the video.</p>

<p align="center">
  <a href="#about">About</a>
  ·
  <a href="#highlights">Highlights</a>
  ·
  <a href="#availability-and-requirements">Availability</a>
  ·
  <a href="#quick-start">Quick start</a>
  ·
  <a href="#controls">Controls</a>
  ·
  <a href="#configuration">Configuration</a>
  ·
  <a href="#limitations">Limitations</a>
</p>

<p align="center">
  <a href="https://github.com/Anthodev/furami/actions/workflows/ci.yml"><img src="https://github.com/Anthodev/furami/actions/workflows/ci.yml/badge.svg?branch=develop" alt="Linux CI"></a>
  <img src="https://img.shields.io/badge/platform-Linux%20x86__64-informational" alt="Linux x86-64">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/Anthodev/furami?label=source%20license" alt="Furami-authored source license"></a>
  <img src="https://img.shields.io/badge/Qt-6.11.2-informational" alt="Qt 6.11.2">
</p>

> [!IMPORTANT]
> Furami is a development prototype with no installer and **no published release**. It is exercised on one Fedora 44 / KDE Plasma Wayland system with an AMD GPU, so treat other setups as untested. The AppImage recipe builds local release candidates for a tag-driven stable and pre-release pipeline, and every publication stays blocked until its source and license gate passes. See [Availability and requirements](#availability-and-requirements) and [Limitations](#limitations).

## About

Furami shows a console that reaches your PC through a USB capture card (UVC) and plays that console's audio through one Furami-owned PipeWire loopback. You choose the real source, resolution, exact frame rate, input format and audio routing in the window — there is no command-line-only selection and no separate Apply step. Every accepted change is applied immediately, and failed or unverified selections are never written to disk.

```text
Game console ──HDMI──► USB capture card ══UVC══► Furami ══► video window
                                                   └──► PipeWire loopback ──► speakers / headphones
```

**The goal is a quiet, immediate desktop viewer that never changes your system mixer or output selection.**

## Highlights

- **Live console video.** Plays the UVC capture mode you select, windowed or fullscreen, through the shared libmpv owner. Furami uses native V4L2 discovery; `v4l2-ctl` is not required.
- **Immediate selection in the window.** Choose a real source and Furami keeps the current compatible mode, or automatically takes the card's largest advertised resolution at its maximum exact frame rate. Input format is **Auto** by default (ranked `YUYV`, then `NV12`, then `MJPG`) with explicit manual overrides, and capture audio is optional.
- **Console audio through one owned loopback.** **Auto** sends audio to EasyEffects' output sink when it is live and routable, otherwise to the current default output. **Manual** pins a sink of your choice and goes silent if it disappears — there is no fallback; reselecting it is up to you.
- **Playback controls.** `Space` freezes the picture and silences output, and resuming jumps to the console's current scene without replaying the pause. Volume and mute act on Furami's playback output only, from the slider, the mouse wheel or the keyboard.
- **Stream statistics.** Source, input format, resolution, FPS and a concise capture-audio label appear at the bottom left while the chrome is shown, with **Options** at the bottom right. Hide only the statistics in **Options → Réglages → Préférences** — errors and recovery messages stay visible.
- **Automatic per-source profiles.** Five fields — resolution, exact rational Framerate, input-format policy, renderer settings and capture input audio — are remembered per capture source. Output routing, volume, mute, fullscreen, statistics visibility and diagnostic logging stay global.
- **Capture recovery.** Reconnects the last applied selection when the same card returns. Paused playback stays paused, audio recovers independently while video keeps running, and ambiguous matches require your choice.
- **Diagnostics you opt in to.** Logging is off by default. When enabled, Furami writes bounded rotating logs and a sanitized environment/session report for troubleshooting.

## Availability and requirements

Furami has **no published release yet**. The [AppImage recipe](packaging/appimage/README.md) builds local release candidates for a tag-driven pipeline that publishes stable and pre-release tags only from a commit whose version and changelog section match the tag; distribution waits on the source and license completeness gate recorded in `release-ready.json`, so treat every candidate as evaluation material rather than a stable download. A candidate's public assets carry the corresponding sources, but the pinned Alpine binary bootstrap and base image stay in a private authenticated cache, so those files are not a complete offline rebuild of the AppImage runtime.

- **Platform.** Linux x86_64. Fedora 44 / KDE Plasma Wayland with an AMD RX 7900 XTX is the exercised setup; Ubuntu and Arch are untested and not required. See the [portability notes](packaging/appimage/PORTABILITY.md).
- **Display.** Wayland sessions need a running XWayland server and a reachable `DISPLAY`. Furami is not a native Wayland-QPA application.
- **Graphics.** A host Vulkan loader and a working ICD for your GPU are required; software rendering is not supported.
- **Audio (optional).** A running host PipeWire session providing the `pw-loopback` and `pw-metadata` helpers (see the [media recipe](packaging/media/README.md)). A standalone PulseAudio setup is not a supported transport, and Furami never starts or installs a PipeWire daemon, session manager or WirePlumber. Video-only viewing works without audio.

## Quick start

Build Furami first — see [Building from source](#building-from-source) — then launch it with no capture arguments:

```sh
./furami
```

With no saved selection Furami starts idle; choose a source, resolution and frame rate in the window and the selection applies right away. The input format defaults to **Auto**.

Command-line selection remains available. Node, size and rate are required together:

```sh
./furami --capture-node /dev/video0 --capture-size 2560x1440 --capture-rate 60/1
```

Omitting `--capture-fourcc`, or passing lowercase `auto`, selects among the chosen node's advertised formats for that exact size and rational rate. This is an image-fidelity heuristic, not a latency or CPU/GPU ranking. Pass an explicit FourCC to pin the input format; manual `BGR3` remains supported but is excluded from Auto:

```sh
./furami --capture-node /dev/video0 --capture-fourcc NV12 --capture-size 2560x1440 --capture-rate 60/1
```

Explicit capture arguments take precedence over automatic restoration for the initial selection. Unknown capabilities stay distinct from unsupported modes: neither Auto nor manual selection silently lowers the size or rate, and a failing open never triggers an alternate-format retry. Advertised support and the resolved request are not proof of actual negotiation, held cadence or image quality. The values above are examples — use what your card supports.

List the available audio sources (through the PipeWire/PulseAudio catalog interface), then pass your capture card's source:

```sh
./furami --list-audio-sources
./furami --capture-node /dev/video0 --capture-fourcc NV12 --capture-size 2560x1440 --capture-rate 60/1 \
  --capture-audio-source alsa_input.usb-MyCard-01.analog-stereo
```

`--capture-audio-off` forces capture audio off for the session.

| Option | Meaning |
| --- | --- |
| `--capture-node <path>` | Capture node to open, e.g. `/dev/video0` |
| `--capture-size <WxH>` | Exact requested frame size |
| `--capture-rate <num/den>` | Exact requested rational frame rate |
| `--capture-fourcc <fourcc>` | Pin the input format; omit or pass `auto` to select automatically |
| `--capture-audio-source <name>` | Capture audio source from the discovery catalog |
| `--capture-audio-off` | Keep capture audio disabled for this session |
| `--list-audio-sources` | Print the available audio sources and exit |

Node, size and rate are required together, and these flags mirror the window selections; `--list-audio-sources` prints the catalog and exits.

## Controls

Move the pointer over the window to reveal the ribbon. **Source**, **Résolution** and **Framerate** select the capture immediately; **Audio** chooses the playback output independently of capture. Open **Options** for capture actions, compatible input formats and renderer controls. The ribbon displays **Auto** as the output label whenever Auto routing is active.

| Input | Action |
| --- | --- |
| `Space` | Pause or resume at video focus, outside text entry and popups |
| `F11` | Toggle fullscreen for the Furami window only, outside text entry |
| `Esc` | Open **Réglages** when no popup is open; otherwise dismiss the ordinary popup, without leaving fullscreen |
| `Tab` / `Shift+Tab` | Move focus between controls |
| **Pause / Resume** action | Same as `Space`, from Options |
| **Volume slider** | Furami's playback volume, 0–100, immediate; arrow keys change it by 1 point |
| **Wheel over video or the volume rail** | Change playback volume by 5 percentage points per notch, bounded to 0–100 |
| **Mute / Unmute** button | Furami's playback output only |
| **Reconnect** button | Retry the last valid selection; join recovery already in progress |

Text entry takes priority: `Space` goes to the field and its validator without pausing; `F11` and `Esc` are consumed without changing fullscreen, playback or the popup. Other focused controls keep their normal `Space` behavior. Mandatory save/reset decisions stay open on `Esc`. Shortcuts use unmodified presses, ignore auto-repeat and never act globally.

Wheel volume works only while Furami is active, no popup is open and no modifier is held. Menus and lists retain their normal scrolling; horizontal or pixel-only input does not change volume. Volume and mute stay independent, including while muted; neither changes the desktop mixer or output selection. Controls fade away after five seconds without pointer activity when no menu is open and no control keeps keyboard focus, and the whole ribbon hides while another application takes focus, so Furami never floats above your work.

## Configuration

Furami stores one strict JSON file at `$XDG_CONFIG_HOME/furami/settings.json` when `XDG_CONFIG_HOME` is an absolute path, or `$HOME/.config/furami/settings.json` when it is unset or empty. An invalid location is reported rather than replaced with a guessed path.

| Scope | Contents | Behavior |
| --- | --- | --- |
| Automatic per-source profile | Resolution, exact rational Framerate, input-format policy, renderer settings, capture input audio | Restored together when a known source is selected. A new source starts with Auto format, its largest eligible advertised mode, highest exact rate, default rendering and uniquely associated card audio when available. There is no profile manager or normal save confirmation. |
| Global preferences | Output routing (Auto/Manual), playback volume, mute, fullscreen, statistics visibility, diagnostic-logging opt-in | Source changes never replace them; they save at a safe close even without capture. |

Profiles bind to an exact raw USB serial plus vendor/product identity when unique. Missing or duplicated serials use the exact USB topology instead, so changing ports can mean a new source; an observed collision stays topology-bound. The physical node binding stays session-only and is never saved as a `/dev/videoN` route.

The strict **schema version 3** document contains `profiles`, `last_source` and `preferences`, with no legacy `applied` payload or filter chains. Legacy version-1 and version-2 files load their other profiles and preferences but their retired filter chains are ignored; the first verified apply or safe close saves format 3, and loading alone does not rewrite the file. Older builds cannot read format 3 — before downgrading, preserve a copy and use a separate configuration, since there is no automatic downgrade.

A corrupt, invalid or newer-version file is preserved and all automatic saves stay blocked until you explicitly confirm **Reset saved file**, which clears all source profiles and the last-source pointer while retaining current global preferences. It does not stop or modify live capture, and only a new verified user selection can create a profile again.

If a saved video mode is no longer advertised, Furami adapts the resolution and rate before opening, retaining a manual format while any eligible mode exists under it; only proven absence of an eligible manual mode permits Auto. Verified successful selections update their profiles; failed selections and unverified candidates do not.

## Diagnostics and privacy

Furami can write diagnostic logs and a sanitized environment/session report to help describe a problem without a debugger. Logging is **off by default**, including for existing configurations without the preference and for settings files Furami refuses.

Enable **Activer les journaux de diagnostic** in **Options → Réglages → Préférences**. The change takes effect immediately, without restarting Furami or the capture. Disabling stops accepting new diagnostic output to files and the technical terminal and hides **Options → Ouvrir le dossier des journaux**; an operation already in progress may finish, and existing logs are kept. Functional command-line results, argument errors, exit codes and Qt's own fatal-message behaviour are unchanged. Older builds that do not recognize this preference refuse settings saved with logging enabled.

The folder is `$XDG_STATE_HOME/furami/logs` when `XDG_STATE_HOME` is an absolute path, or `$HOME/.local/state/furami/logs` when it is unset or empty (with `HOME` absolute). A non-empty relative `XDG_STATE_HOME` is reported as an error rather than replaced with a guessed path. Storage is bounded to one active file plus four rotations, each at most 1 MiB.

Reports omit raw capture-device paths, USB serial/topology properties and raw native/Qt messages. Technical audio endpoint names, device VID/PID, capture modes and diagnostic codes are retained for troubleshooting, so review a report before sharing it.

## Limitations

- **No named profile manager.** Profiles are automatic per source; there is no naming, import/export or cross-source copying.
- **Active-capture profile qualification is blocked.** Capture-free file migration and relaunch pass, but isolated capture crashed during close at 120 FPS and in the single authorized 60-FPS candidate attempt, and the pre-profiles baseline crashed under the identical private 60-FPS setup. Stacks locate the failure in the RADV/libplacebo/mpv upload path; the root cause and its relation to this feature remain unresolved, so successful active-profile saving/relaunch and startup adaptation are not yet qualified.
- **Recovery qualification is pending.** Physical unplug/replug, recovery while paused, audio-only loss and post-recovery A/V still need live hardware verification.
- **Narrowly tested.** Fedora 44 / KDE Plasma Wayland with an AMD RX 7900 XTX is the exercised setup; Ubuntu and Arch are untested. See the [portability notes](packaging/appimage/PORTABILITY.md).
- **Hardware Vulkan is required.** Wayland sessions also need XWayland; software rendering is not supported.
- **Audio routing is a prototype pending extended qualification.** Basic picture, sound and capture controls are user-confirmed on the tested setup; physical device-loss scenarios and sustained live A/V behaviour still need qualification.
- **Frame rate is playback configuration, not a measurement.** Furami derives mpv playback timing from the selected rate (for example `60/1` or `60000/1001`) and shows it as application-configured timing metadata. The card's real capture cadence is neither measured nor certified, and it may renegotiate internally.
- **Audio and video clocks are separate.** There is no common media clock; A/V synchronisation, drift and latency over sustained playback still need measurement.
- **Low latency is the goal, not a claim.** Furami promises no latency figure, the physical latency has not been measured, and long-session stability remains unqualified.
- **Diagnostic logs are best-effort, not an audit trail.** Rotation and retention are bounded by design, so loss is counted and reported rather than guaranteed absent; this is not a crash- or power-loss-proof journal.

## Building from source

For contributors, Furami uses Rust and Qt. The [media recipe](packaging/media/README.md) covers the pinned dependencies and prerequisites; the [AppImage documentation](packaging/appimage/README.md) covers application builds, the release-candidate chain and packaging.

## Acknowledgements

Furami's overlay icons come from [Tabler Icons](https://tabler.io/icons) (see `qml/icons/LICENSE-tabler.txt`), and its default palette is inspired by [Catppuccin](https://catppuccin.com/) Macchiato.

Contributions are welcome through issues and pull requests against the `develop` branch. Nothing here is a support commitment: the exercised configuration stays the one documented above.

## License

Furami-authored source is [MIT](LICENSE) — the badge above reports that source license only. The pinned media build includes GPL mpv/FFmpeg and LGPL libplacebo/Qt, so a combined executable is not MIT-only; the [core license audit](packaging/media/LICENSE-AUDIT.md) records build flags and distribution obligations for redistribution.
