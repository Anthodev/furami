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
- **Audio defaults that follow the card.** For a new configuration, Furami automatically enables the card's own audio input when one unique match ties it to the selected USB device, and keeps the output on **Auto** at 100 %, unmuted. Choices you have saved — audio off, mute or zero volume — stay respected, and `--capture-audio-off` still wins.
- **Fullscreen.** `F11` toggles fullscreen and restores your previous window size and focus.
- **Immediate volume and mute.** Hover the right-side audio control to reveal the slider; volume and mute change Furami's playback output right away — never the capture device or system mixer.
- **Quiet overlay controls.** A soft Brume ribbon uses a Catppuccin Macchiato-inspired default palette and appears on pointer movement. Controls fade away after five seconds without movement when no menu is open and no control retains keyboard focus, and the whole ribbon hides while another application takes focus so Furami never floats above your work. Prune and Craie remain future themes.
- **Stream statistics.** Source, input format, resolution, FPS and a concise capture-audio label appear at the bottom left while the chrome is shown; **Options** stays at the bottom right. Hide only these statistics in **Options → Réglages → Préférences** — errors and recovery messages remain visible.
- **Capture selection in the window.** Choose a real source and the window keeps the current compatible mode, or automatically takes the card's largest advertised resolution at its maximum exact frame rate. Changing any choice applies right away, and options also expose Auto/manual input format and capture audio.
- **Filters and renderer settings.** Edit video filters inside the same Options frame: every loaded entry exposes all of its typed parameters — bounded numbers, whole-number tap counts and enum choices — alongside **Monter**/**Descendre** reordering that keeps labels, parameters and enabled flags intact, including disabled and repeated entries, a per-entry **Retirer** action distinct from the enable toggle, and **Vider la chaîne** for a valid empty chain. Each complete numeric entry, choice, move or clear commits one immediate, verified whole-chain transaction, and a verified change is remembered by the selected source's automatic profile. Renderer settings keep their separate editor in the same frame, unchanged in scope, and Options and its settings pages are 20% wider, bounded by the available window width.
- **Capture recovery.** Reconnects the last applied selection when the same card returns; paused playback stays paused, and ambiguous matches require your choice.
- **Audio-only recovery.** Keeps video running silently while waiting for the selected audio source, without choosing a different input.
- **Saved settings.** Restores the last successfully applied capture and keeps the Auto/Manual output preference, local volume, mute, fullscreen and statistics-visibility settings across safe closes.

## First use

Launch `./furami` without capture arguments to choose a source, resolution and frame rate in the window, then apply the draft. With no saved selection, Furami starts idle; the input format defaults to **Auto**.

Command-line selection remains available. The node, size and rate are required together:

```sh
./furami --capture-node /dev/video0 --capture-size 2560x1440 --capture-rate 60/1
```

Omitting `--capture-fourcc`, or setting it to lowercase `auto`, selects among the chosen node's advertised formats for that exact size and rational rate, preferring `YUYV`, then `NV12`, then `MJPG`. This is an image-fidelity heuristic, not a latency or CPU/GPU ranking. Furami uses native V4L2 discovery; `v4l2-ctl` is not required.

Pass an explicit FourCC to pin the input format; manual `BGR3` remains supported but is excluded from Auto:

```sh
./furami --capture-node /dev/video0 --capture-fourcc NV12 --capture-size 2560x1440 --capture-rate 60/1
```

Unknown capabilities remain distinct from unsupported modes. Neither Auto nor manual selection silently lowers the size or rate, and an opening failure never triggers an alternate-format retry. Advertised support and the resolved input request are not proof of actual negotiation, held cadence or image quality.

The values above are examples — use what your card actually supports. With no saved capture and no explicit selection, the window opens idle. Explicit capture arguments take precedence over automatic restoration until you apply them.

Audio is optional. It requires a running host PipeWire session with `pw-loopback` and `pw-metadata` installed at the usual locations; Furami never starts or installs a PipeWire daemon, session manager, or WirePlumber. List the available sources (through the PipeWire/PulseAudio catalog interface), then replace the example source name with your capture card's source:

```sh
./furami --list-audio-sources
./furami --capture-node /dev/video0 --capture-fourcc NV12 --capture-size 2560x1440 --capture-rate 60/1 \
  --capture-audio-source alsa_input.usb-MyCard-01.analog-stereo
```

## Using it

Move the pointer over the window to reveal the ribbon. **Source**, **Résolution** and **Framerate** select the capture immediately — **Annuler** is no longer part of the ribbon. **Audio** chooses the playback output right away, independently of capture and Apply. Open **Options** for capture actions, including capture-audio input, compatible input formats, filters and renderer controls.

The ribbon also displays **Auto** as the output label when Auto routing is active, whichever cards or sinks resolve. Normal launches automatically reopen the saved selection when its requested sources and exact mode are available; otherwise the window shows the selection and the reason without starting a stream. Pause or resume with `Space` or the Options action. Adjust playback volume and mute on the right, toggle fullscreen with `F11` or the ribbon button, and hide Furami underneath another application — its controls disappear there without stealing focus. Closing capture returns to idle; restarting fully reopens it. Reconnect retries the last valid selection after a failure and leaves a healthy session unchanged.

Resuming briefly reconnects, then shows the console's current scene.

## Controls

| Input | Action |
| --- | --- |
| `Space` | Pause or resume, when the video or window has focus |
| `F11` | Toggle fullscreen for the Furami window only |
| `Esc` | Dismiss the open menu or leave fullscreen |
| `Tab` / `Shift+Tab` | Move focus between controls |
| **Pause / Resume** action | Same as `Space`, from Options |
| **Volume slider** | Furami's playback volume, 0–100, immediate |
| **Mute / Unmute** button | Furami's playback output only |
| **Reconnect** button | Retry the last valid selection; join recovery already in progress |

Focused controls keep their keys: `Space` types a space or activates the focused control instead of pausing. No global shortcuts; fullscreen and focus stay local to the window.

## Saved settings

Furami silently remembers a separate **automatic profile for each capture source**: resolution, exact rational Framerate, input-format policy, complete filter chain, renderer settings and input audio. Selecting a known source restores those six fields together; a new source starts independently with Auto format, its largest eligible advertised mode and highest exact rate, empty filters, default rendering and uniquely associated card audio when available. There is no profile manager or normal save confirmation.

**Output routing, volume, mute, fullscreen and statistics visibility stay global.** Source changes never replace them; they save at safe close even without capture. Verified successful selections update their profiles; failed selections and unverified candidates do not. The last successful source is restored at startup when identity and validation permit, without selecting another card if it is absent.

If a saved video mode is no longer advertised, Furami adapts the resolution and rate before opening, retaining a manual format while any eligible mode exists under it. Only proven absence of an eligible manual mode permits Auto. Missing saved audio or rejected treatments remain visible errors, not silent substitutions. A verified startup adaptation is saved; an unchanged startup does not itself rewrite a profile.

Profiles follow an exact raw USB serial plus vendor/product identity when unique. Missing serials and observed duplicate serials use the exact USB topology instead; changing ports can then mean a new source. An observed collision stays topology-bound. These profile rules do not relax capture-device admission or resolve ambiguous devices.

The strict **schema version 2** document contains `profiles`, `last_source` and `preferences`, with no legacy `applied` payload. Version 1 files load strictly into memory and migrate only at the next authorized save, including safe close. Older binaries refuse version 2 without changing it: preserve a copy and use a separate configuration before downgrading, or explicitly reset the file and lose its saved profiles. There is no automatic downgrade.

The input-format policy is separate from audio output. Auto is resolved afresh for a full capture selection; restart, resume and recovery retain the admitted concrete format and physical node/USB binding. That binding is session-only and never saved as a `/dev/videoN` route. A new process validates the profile afresh. Invalid Auto FourCC values are refused with the original preserved and automatic saves blocked; manual `BGR3` remains valid.

The file is `$XDG_CONFIG_HOME/furami/settings.json` when `XDG_CONFIG_HOME` is an absolute path. If that variable is unset or empty, Furami uses `$HOME/.config/furami/settings.json`. An invalid location is reported rather than replaced with a guessed path.

A corrupt, invalid or newer-version file is preserved. All automatic saves remain blocked; preference changes are session-only until you explicitly confirm **Reset saved file**. Reset clears **all source profiles and the last-source pointer**, retaining current global preferences. It does not stop or modify live capture; only a new verified user selection can create a profile again.

## Limitations

- **No named profile manager.** Profiles are automatic per source; there is no profile naming, import/export or cross-source copying.
- **Active-capture profile qualification is blocked.** Automated checks and capture-free file migration/relaunch pass. Isolated ShadowCast NV12 capture crashed during close at 120 FPS and in the single authorized 60-FPS candidate attempt; the pre-profiles baseline also crashed under the identical private 60-FPS verification setup. Stacks locate the failure in RADV/libplacebo/mpv upload, but the root cause and its relation to this feature remain unresolved. Successful active-profile saving/relaunch and startup adaptation are not yet qualified.
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
