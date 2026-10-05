#[cfg(not(any(cxxqt_qt_version_at_least_6_8, cxxqt_qt_version_at_least_7)))]
compile_error!("Furami requires Qt 6.8 or newer");

// Keep the library's generated QML registration linked into the executable.
extern crate furami as _;

mod diagnostics;
mod profiles;
use furami::{
    app::settings::{PersistenceSession, decide_startup, resolve_settings_path},
    capture::{
        audio,
        input::{CaptureArgumentError, CaptureArguments, CaptureSelection, SelectionError},
        linux,
    },
    domain::capture::{
        AudioError, AudioSelection, AudioSourceIdentity, ObservationEpoch, WatchId, WatchStamp,
    },
    domain::state::DraftSettings,
};
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use tracing_subscriber::EnvFilter;

#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("RUST_LOG must be valid UTF-8")]
    InvalidLogEnvironment(#[source] std::env::VarError),
    #[error("invalid RUST_LOG filter: {0}")]
    InvalidLogFilter(#[source] tracing_subscriber::filter::ParseError),
    #[error("failed to install tracing subscriber: {0}")]
    SubscriberInstallation(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error(
        "missing or empty DISPLAY: install/enable XWayland in your Wayland session, or set DISPLAY to a reachable X11 display"
    )]
    NoDisplay,
    #[error("DISPLAY must be valid UTF-8")]
    InvalidDisplayEnvironment(#[source] std::env::VarError),
    #[error("FURAMI_MEDIA_PREFIX must name the frozen media prefix: {0}")]
    MediaPrefixEnvironment(#[source] std::env::VarError),
    #[error("FURAMI_MEDIA_PREFIX must be an absolute existing directory")]
    InvalidMediaPrefix,
    #[error(transparent)]
    NativeLaunch(#[from] furami::native_host::NativeLaunchError),
    #[error(transparent)]
    CaptureArguments(#[from] CaptureArgumentError),
    #[error("capture startup Prevalidation requested {request}: {source}")]
    CaptureDiscovery {
        request: String,
        #[source]
        source: linux::CaptureError,
    },
    #[error("capture startup Prevalidation requested {request}: {source}")]
    CaptureSelection {
        request: String,
        #[source]
        source: SelectionError,
    },
    #[error(transparent)]
    Audio(#[from] AudioError),
    #[error(
        "capture audio needs PULSE_PROP_node.dont-fallback=true in this process: re-exec failed: {0}"
    )]
    NoFallbackExec(#[source] std::io::Error),
}

/// The capture process must run with the process-local PulseAudio client
/// property `PULSE_PROP_node.dont-fallback=true`, otherwise libpulse routes a
/// captured stream to the default source when the requested source disappears.
/// The property must already be present in the process environment *before*
/// any logging, Qt, libpulse or worker setup, so the very first action re-execs
/// this exact executable with the same arguments plus the variable set. The
/// replacement is `exec`, not a child process: there is no supervisor and no
/// shell. Only the exact value `true` short-circuits; any other value is
/// replaced with `true`. The variable is process-local and is deliberately not
/// written back to any parent shell, and no default audio device/output
/// variable is set.
fn ensure_capture_no_fallback_environment() -> Result<(), StartupError> {
    if no_fallback_already_set(std::env::var_os("PULSE_PROP_node.dont-fallback").as_deref()) {
        return Ok(());
    }
    let executable = std::env::current_exe().map_err(StartupError::NoFallbackExec)?;
    let error = std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .env("PULSE_PROP_node.dont-fallback", "true")
        .exec();
    // `exec` only returns on failure; the process image was not replaced.
    Err(StartupError::NoFallbackExec(error))
}

fn no_fallback_already_set(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("true"))
}

fn main() -> anyhow::Result<()> {
    ensure_capture_no_fallback_environment()?;
    initialize_logging()?;
    run()?;
    Ok(())
}

fn initialize_logging() -> Result<(), StartupError> {
    let directives = match std::env::var("RUST_LOG") {
        Ok(value) if !value.is_empty() => value,
        Ok(_) | Err(std::env::VarError::NotPresent) => "info".to_owned(),
        Err(error) => return Err(StartupError::InvalidLogEnvironment(error)),
    };
    let filter = EnvFilter::try_new(directives).map_err(StartupError::InvalidLogFilter)?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(StartupError::SubscriberInstallation)
}

#[derive(Debug)]
struct StartupArguments {
    video: Option<CaptureArguments>,
    audio_source: Option<String>,
    audio_off: bool,
    list_audio_sources: bool,
    qualification_stdin: bool,
}

impl StartupArguments {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, StartupError> {
        let mut args = args.into_iter();
        let mut video_args = Vec::new();
        let mut audio_source = None;
        let (mut audio_off, mut list_audio_sources, mut qualification_stdin) =
            (false, false, false);
        while let Some(flag) = args.next() {
            let text = flag.to_str().ok_or_else(|| {
                CaptureArgumentError::InvalidArgument("flag must be UTF-8".into())
            })?;
            match text {
                "--capture-audio-source" => {
                    if audio_source.is_some() {
                        return Err(CaptureArgumentError::InvalidArgument(
                            "duplicate --capture-audio-source".into(),
                        )
                        .into());
                    }
                    let name = args
                        .next()
                        .and_then(|value| value.into_string().ok())
                        .filter(|value| !value.starts_with("--"))
                        .ok_or_else(|| {
                            CaptureArgumentError::InvalidArgument(
                                "missing UTF-8 value for --capture-audio-source".into(),
                            )
                        })?;
                    // Validate names before discovery. Special default aliases never
                    // enter a selection, including the disabled retained-source mode.
                    AudioSourceIdentity::new(name.clone(), Vec::new())?;
                    audio_source = Some(name);
                }
                "--capture-audio-off" | "--list-audio-sources" | "--qualification-stdin" => {
                    let slot = match text {
                        "--capture-audio-off" => &mut audio_off,
                        "--list-audio-sources" => &mut list_audio_sources,
                        _ => &mut qualification_stdin,
                    };
                    if *slot {
                        return Err(CaptureArgumentError::InvalidArgument(format!(
                            "duplicate {text}"
                        ))
                        .into());
                    }
                    *slot = true;
                }
                "--capture-node" | "--capture-fourcc" | "--capture-size" | "--capture-rate" => {
                    let value = args.next().ok_or_else(|| {
                        CaptureArgumentError::InvalidArgument(format!("missing value for {text}"))
                    })?;
                    video_args.push(flag);
                    video_args.push(value);
                }
                _ => {
                    return Err(CaptureArgumentError::InvalidArgument(format!(
                        "unknown option {text:?}"
                    ))
                    .into());
                }
            }
        }
        Ok(Self {
            video: CaptureArguments::parse(video_args)?,
            audio_off: audio_off || audio_source.is_none(),
            audio_source,
            list_audio_sources,
            qualification_stdin,
        })
    }
}

fn run() -> Result<(), StartupError> {
    let arguments = StartupArguments::parse(std::env::args_os().skip(1))?;
    if arguments.list_audio_sources {
        for source in audio::discover()? {
            println!("{}\t{}", source.identity.name(), source.description);
        }
        return Ok(());
    }
    let mut persistence = PersistenceSession::load(resolve_settings_path(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    ));
    let saved = persistence.saved_selection();
    let restore_audio = arguments.video.is_none()
        && saved
            .as_ref()
            .is_some_and(|settings| settings.audio.enabled());
    let mut restore_audio_error = None;
    let audio_sources = if arguments.audio_source.is_some()
        || restore_audio
        || (arguments.qualification_stdin && arguments.video.is_some())
    {
        match audio::discover() {
            Ok(sources) => sources,
            Err(error) if arguments.audio_source.is_none() => {
                restore_audio_error = Some(format!(
                    "Cannot establish saved audio source inventory: {error}"
                ));
                tracing::warn!(%error, "startup_audio_catalog_unavailable");
                Vec::new()
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        Vec::new()
    };
    let audio = match arguments.audio_source.as_deref() {
        Some(name) => {
            let source = audio::select(&audio_sources, name)?;
            if arguments.audio_off {
                AudioSelection::Disabled {
                    retained: Some(source),
                }
            } else {
                AudioSelection::Enabled { source }
            }
        }
        None => AudioSelection::default(),
    };
    let capture = arguments
        .video
        .map(|args| {
            let snapshot = linux::discover().map_err(|source| StartupError::CaptureDiscovery {
                request: args.to_string(),
                source,
            })?;
            let selection = CaptureSelection::from_snapshot(&snapshot, &args.node, args.mode)
                .map_err(|source| StartupError::CaptureSelection {
                    request: args.to_string(),
                    source,
                })?;
            let requested = selection.requested();
            Ok::<_, StartupError>(DraftSettings {
                video: furami::domain::capture::ModeRequest {
                    identity: requested.identity,
                    mode: requested.mode,
                },
                audio,
            })
        })
        .transpose()?;
    let sources: Vec<_> = audio_sources
        .into_iter()
        .map(|source| source.identity)
        .collect();
    let startup = decide_startup(
        saved,
        capture,
        |settings| {
            let snapshot = linux::discover().map_err(|error| error.to_string())?;
            let stamp = WatchStamp {
                watch: WatchId::new(1).ok_or("invalid initial preflight watch")?,
                epoch: ObservationEpoch::new(1).ok_or("invalid initial preflight epoch")?,
            };
            furami::capture::validate_prepared(settings.clone(), &snapshot, &[], stamp)
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
        restore_audio_error.map_or_else(|| Ok(sources.as_slice()), Err),
    );
    persistence.prepare_startup(&startup);
    let x11_display = match std::env::var("DISPLAY") {
        Ok(value) if !value.is_empty() => value,
        Ok(_) | Err(std::env::VarError::NotPresent) => return Err(StartupError::NoDisplay),
        Err(error) => return Err(StartupError::InvalidDisplayEnvironment(error)),
    };
    let media_prefix =
        std::env::var("FURAMI_MEDIA_PREFIX").map_err(StartupError::MediaPrefixEnvironment)?;
    let prefix = std::path::Path::new(&media_prefix);
    if !prefix.is_absolute() || !prefix.is_dir() {
        return Err(StartupError::InvalidMediaPrefix);
    }
    tracing::info!(
        x11_display = x11_display.as_str(),
        media_prefix = media_prefix.as_str(),
        "starting Qt application"
    );
    furami::native_host::run_application(
        &media_prefix,
        &x11_display,
        startup,
        persistence,
        sources,
        arguments.qualification_stdin,
    )?;
    tracing::info!("Qt application exited");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn capture_cli_requires_complete_explicit_tuple_and_checked_values() {
        assert!(StartupArguments::parse(args(&[])).unwrap().video.is_none());
        let parsed = StartupArguments::parse(args(&[
            "--capture-node",
            "/dev/video0",
            "--capture-fourcc",
            "NV12",
            "--capture-size",
            "2560x1440",
            "--capture-rate",
            "60/1",
        ]))
        .unwrap()
        .video
        .unwrap();
        assert_eq!(parsed.node, std::path::Path::new("/dev/video0"));
        assert_eq!(parsed.mode.rate.numerator(), 60);
        assert_eq!(parsed.mode.rate.denominator(), 1);
        assert!(matches!(
            StartupArguments::parse(args(&["--capture-node", "/dev/video0"])),
            Err(StartupError::CaptureArguments(
                CaptureArgumentError::IncompleteSelection
            ))
        ));
        for value in ["0x1440", "2560x0", "2560x1440x1"] {
            assert!(
                StartupArguments::parse(args(&[
                    "--capture-node",
                    "/dev/video0",
                    "--capture-fourcc",
                    "NV12",
                    "--capture-size",
                    value,
                    "--capture-rate",
                    "60/1"
                ]))
                .is_err()
            );
        }
        assert!(
            StartupArguments::parse(args(&[
                "--capture-node",
                "/dev/video0",
                "--capture-fourcc",
                "NV12",
                "--capture-size",
                "2560x1440",
                "--capture-rate",
                "59.94"
            ]))
            .is_err()
        );
    }

    #[test]
    fn capture_cli_rejects_duplicates_unknown_flags_and_missing_values() {
        for values in [
            vec!["--capture-node"],
            vec![
                "--capture-node",
                "/dev/video0",
                "--capture-node",
                "/dev/video2",
            ],
            vec!["--proof-source", "testsrc2"],
        ] {
            assert!(StartupArguments::parse(args(&values)).is_err());
        }
    }

    #[test]
    fn audio_cli_is_explicit_and_can_start_disabled_with_retained_source() {
        let defaults = StartupArguments::parse(args(&[])).unwrap();
        assert!(defaults.audio_source.is_none() && defaults.audio_off);
        assert!(!defaults.list_audio_sources && !defaults.qualification_stdin);
        let enabled =
            StartupArguments::parse(args(&["--capture-audio-source", "exact.usb.source"])).unwrap();
        assert_eq!(enabled.audio_source.as_deref(), Some("exact.usb.source"));
        assert!(!enabled.audio_off);
        let disabled = StartupArguments::parse(args(&[
            "--capture-audio-source",
            "exact.usb.source",
            "--capture-audio-off",
            "--qualification-stdin",
        ]))
        .unwrap();
        assert_eq!(disabled.audio_source.as_deref(), Some("exact.usb.source"));
        assert!(disabled.audio_off && disabled.qualification_stdin);
        assert!(
            StartupArguments::parse(args(&["--list-audio-sources"]))
                .unwrap()
                .list_audio_sources
        );
    }

    #[test]
    fn audio_cli_rejects_duplicate_flags_aliases_and_missing_values() {
        for values in [
            vec!["--capture-audio-source"],
            vec!["--capture-audio-source", ""],
            vec!["--capture-audio-source", "@DEFAULT_SOURCE@"],
            vec![
                "--capture-audio-source",
                "exact",
                "--capture-audio-source",
                "other",
            ],
            vec!["--capture-audio-off", "--capture-audio-off"],
            vec!["--qualification-stdin", "--qualification-stdin"],
            vec!["--list-audio-sources", "--list-audio-sources"],
            vec!["--capture-audio-source", "--capture-audio-off"],
        ] {
            assert!(
                StartupArguments::parse(args(&values)).is_err(),
                "{values:?}"
            );
        }
    }
}
