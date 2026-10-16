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
- **Renderer settings.** Tune scaling, dithering, debanding and interpolation in **Options → Réglages → Rendu**. Changes use the existing verified transaction and are remembered by the selected source's automatic profile.
- **Capture recovery.** Reconnects the last applied selection when the same card returns; paused playback stays paused, and ambiguous matches require your choice.
- **Audio-only recovery.** Keeps video running silently while waiting for the selected audio source, without choosing a different input.
- **Saved settings.** Restores the last successfully applied capture and keeps the Auto/Manual output preference, local volume, mute, fullscreen, statistics-visibility and diagnostic-logging settings across safe closes.
- **Diagnostic logs you opt in to.** Diagnostic logging is **off by default**; enable **Activer les journaux de diagnostic** in **Options → Réglages → Préférences** for bounded troubleshooting logs and a sanitized environment/session report. Turning it off stops accepting new diagnostic output and hides the log-folder action; existing logs are kept. While enabled, choose **Options → Ouvrir le dossier des journaux** to request opening the log folder.

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

Move the pointer over the window to reveal the ribbon. **Source**, **Résolution** and **Framerate** select the capture immediately — **Annuler** is no longer part of the ribbon. **Audio** chooses the playback output right away, independently of capture and Apply. Open **Options** for capture actions, including capture-audio input, compatible input formats and renderer controls.

The ribbon also displays **Auto** as the output label when Auto routing is active, whichever cards or sinks resolve. Normal launches automatically reopen the saved selection when its requested sources and exact mode are available; otherwise the window shows the selection and the reason without starting a stream. Pause or resume with `Space` or the Options action. Adjust playback volume and mute on the right, toggle fullscreen with `F11` or the ribbon button, and hide Furami underneath another application — its controls disappear there without stealing focus. Closing capture returns to idle; restarting fully reopens it. Reconnect retries the last valid selection after a failure and leaves a healthy session unchanged.

Resuming briefly reconnects, then shows the console's current scene.

## Controls

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

Wheel volume works only while Furami is active, no popup is open and no modifier is held. Menus and lists retain their normal scrolling; horizontal or pixel-only input does not change volume. Volume and mute stay independent, including while muted; neither changes the desktop mixer or output selection.

## Saved settings

Furami silently remembers a separate **automatic profile for each capture source**: resolution, exact rational Framerate, input-format policy, renderer settings and input audio. Selecting a known source restores those five fields together; a new source starts independently with Auto format, its largest eligible advertised mode and highest exact rate, default rendering and uniquely associated card audio when available. There is no profile manager or normal save confirmation.

**Video filters have been removed.** Existing version-1 and version-2 settings keep their other profiles and preferences, but their old filter chains are ignored. The first verified apply or safe close saves format 3 without those chains; older builds cannot read that format, and switching back to an old binary will not recover the removed chains. Loading alone does not rewrite the file.

**Output routing, volume, mute, fullscreen, statistics visibility and the diagnostic-logging opt-in stay global.** Source changes never replace them; they save at safe close even without capture. Verified successful selections update their profiles; failed selections and unverified candidates do not. The last successful source is restored at startup when identity and validation permit, without selecting another card if it is absent.

If a saved video mode is no longer advertised, Furami adapts the resolution and rate before opening, retaining a manual format while any eligible mode exists under it. Only proven absence of an eligible manual mode permits Auto. Missing saved audio or rejected treatments remain visible errors, not silent substitutions. A verified startup adaptation is saved; an unchanged startup does not itself rewrite a profile.

Profiles follow an exact raw USB serial plus vendor/product identity when unique. Missing serials and observed duplicate serials use the exact USB topology instead; changing ports can then mean a new source. An observed collision stays topology-bound. These profile rules do not relax capture-device admission or resolve ambiguous devices.

The strict **schema version 3** document contains `profiles`, `last_source` and `preferences`, with no legacy `applied` payload or filter chains. Legacy version-1 and version-2 files follow the lazy migration described above. Before downgrading, preserve a copy and use a separate configuration; there is no automatic downgrade.

The input-format policy is separate from audio output. Auto is resolved afresh for a full capture selection; restart, resume and recovery retain the admitted concrete format and physical node/USB binding. That binding is session-only and never saved as a `/dev/videoN` route. A new process validates the profile afresh. Invalid Auto FourCC values are refused with the original preserved and automatic saves blocked; manual `BGR3` remains valid.

The file is `$XDG_CONFIG_HOME/furami/settings.json` when `XDG_CONFIG_HOME` is an absolute path. If that variable is unset or empty, Furami uses `$HOME/.config/furami/settings.json`. An invalid location is reported rather than replaced with a guessed path.

A corrupt, invalid or newer-version file is preserved. All automatic saves remain blocked; preference changes are session-only until you explicitly confirm **Reset saved file**. Reset clears **all source profiles and the last-source pointer**, retaining current global preferences. It does not stop or modify live capture; only a new verified user selection can create a profile again.

## Diagnostic logs

Furami can write diagnostic logs and a sanitized environment/session report to help describe a problem
without a debugger. Logging is **off by default**, including existing configurations without the preference
and settings files that Furami refuses. Enable **Activer les journaux de diagnostic** in
**Options → Réglages → Préférences** to opt in.

The change takes effect immediately, without restarting Furami or the capture. Disabling stops accepting
new diagnostic output to files and the technical terminal, and hides **Options → Ouvrir le dossier des journaux**.
An operation already in progress may finish; Furami does not wait for it before returning control.
Existing logs are kept. Functional command-line results, argument errors, exit codes and Qt's own
fatal-message behaviour are unchanged. The preference is saved at a verified apply or safe close.
Older builds that do not recognize this preference refuse settings saved with logging enabled.

Reports omit raw capture-device paths, USB serial/topology properties and raw native/Qt
messages. Technical audio endpoint names, device VID/PID, capture modes and diagnostic codes are retained
for troubleshooting; review a report before sharing it.

The folder is `$XDG_STATE_HOME/furami/logs` when `XDG_STATE_HOME` is an absolute path, or
`$HOME/.local/state/furami/logs` when it is unset or empty (with `HOME` absolute). A non-empty relative
`XDG_STATE_HOME` is reported as an error rather than replaced with a guessed path.
Storage is bounded to one active file plus four rotations, each at most 1 MiB. Logging faults and lost
records are reported separately from capture status. See the [UI architecture guide](docs/ui-ux/architecture.md)
for the technical contracts.

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
- **Diagnostic logs are best-effort, not an audit trail.** Rotation and retention are bounded by design, so loss is counted and reported rather than guaranteed absent; this is not a crash- or power-loss-proof journal.

## Availability

Furami is not yet available as a release-ready download. The [AppImage recipe](packaging/appimage/README.md) builds development packages; the AppImage source/license completeness gate is not yet cleared.

## Building from source

For contributors, Furami uses Rust and Qt. The [media recipe](packaging/media/README.md) covers the pinned dependencies and prerequisites; the [AppImage documentation](packaging/appimage/README.md) covers application builds and packaging.

## License

Furami-authored source is [MIT](LICENSE). The pinned media build includes GPL mpv/FFmpeg and LGPL libplacebo/Qt, so a combined executable is not MIT-only; the [core license audit](packaging/media/LICENSE-AUDIT.md) records build flags and distribution obligations for redistribution.
