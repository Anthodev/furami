#[cfg(not(any(cxxqt_qt_version_at_least_6_8, cxxqt_qt_version_at_least_7)))]
compile_error!("Furami requires Qt 6.8 or newer");

// Keep the library's generated QML registration linked into the executable.
extern crate furami as _;

mod profiles;
use furami::diagnostics::report::{ReportSnapshot, StartupFailure, StartupTuple};
use furami::diagnostics::{DiagnosticsConfig, DiagnosticsHandle, DiagnosticsRuntime};
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
use std::process::ExitCode;

#[derive(Debug, thiserror::Error)]
enum StartupError {
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
    #[error("capture startup discovery failed")]
    CaptureDiscovery {
        request: furami::domain::capture::CaptureModeRequest,
        #[source]
        source: linux::CaptureError,
    },
    #[error("capture startup selection failed")]
    CaptureSelection {
        request: furami::domain::capture::CaptureModeRequest,
        #[source]
        source: SelectionError,
    },
    #[error(transparent)]
    Audio(#[from] AudioError),
    #[error(
        "capture audio needs PULSE_PROP_node.dont-fallback=true in this process: re-exec failed: {0}"
    )]
    NoFallbackExec(#[source] std::io::Error),
    #[error("capture startup failed")]
    WithRequest {
        request: furami::domain::capture::CaptureModeRequest,
        #[source]
        source: Box<StartupError>,
    },
    #[error(
        "qualification_arguments: qualification video cannot be combined with capture arguments"
    )]
    QualificationArguments,
    #[error(transparent)]
    QualificationVideo(#[from] furami::media::qualification::QualificationVideoError),
    #[error("qualification_renderer: default renderer could not be compiled")]
    QualificationRenderer,
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

fn main() -> ExitCode {
    let arguments = match StartupArguments::parse(std::env::args_os().skip(1)) {
        Ok(arguments) => arguments,
        Err(error) => {
            let failure = error.diagnostic();
            eprintln!(
                "furami: startup failed ({} at {})",
                failure.code, failure.stage
            );
            return ExitCode::FAILURE;
        }
    };
    let qualification = arguments.qualification_video.is_some();
    if !qualification && ensure_capture_no_fallback_environment().is_err() {
        eprintln!("furami: startup failed (NoFallbackExec)");
        return ExitCode::FAILURE;
    }
    let persistence = if qualification {
        None
    } else {
        Some(PersistenceSession::load(resolve_settings_path(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )))
    };
    let diagnostics = DiagnosticsRuntime::start(DiagnosticsConfig {
        logging_enabled: qualification
            || persistence
                .as_ref()
                .is_some_and(|session| session.preferences().logging_enabled),
        ..DiagnosticsConfig::default()
    });
    diagnostics.install_tracing();
    let handle = diagnostics.handle();
    if let Some(report) = initial_report(&handle) {
        handle.publish_report(report);
    }
    let result = match persistence {
        Some(persistence) => run(arguments, handle.clone(), persistence),
        None => run_qualification(arguments, handle.clone()),
    };
    if let Err(error) = &result {
        let failure = error.diagnostic();
        let code = failure.code;
        let stage = failure.stage;
        eprintln!("furami: startup failed ({code} at {stage})");
        if let Some(detail) = error.plugin_detail() {
            eprintln!("furami: {detail}");
        }
        if let Some(mut report) = initial_report(&handle) {
            report.session.phase = "StartupFailed".into();
            report.session.startup_failure = Some(failure);
            handle.publish_report(report);
        }
        tracing::error!(event_code = "startup_failed", code, stage);
    }
    // Qt returns only after its existing owner acknowledgement/teardown barrier.
    diagnostics.begin_shutdown();
    diagnostics.finish_shutdown(std::time::Duration::from_millis(250));
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn initial_report(diagnostics: &DiagnosticsHandle) -> Option<ReportSnapshot> {
    let revision = diagnostics.reserve_report_revision()?;
    let mut report = ReportSnapshot::initial();
    report.revision = revision;
    report.environment.os = diagnostics.os_report();
    report.environment.log_location = diagnostics.health();
    Some(report)
}

impl StartupError {
    fn plugin_detail(&self) -> Option<&str> {
        match self {
            Self::WithRequest { source, .. } => source.plugin_detail(),
            Self::NativeLaunch(native)
                if matches!(
                    native.failure_kind,
                    furami::native_host::NativeFailureKind::QtPluginMissing
                        | furami::native_host::NativeFailureKind::QtPluginInvalid
                        | furami::native_host::NativeFailureKind::QtPluginLoad
                ) =>
            {
                Some(&native.safe_detail)
            }
            _ => None,
        }
    }
    fn diagnostic(&self) -> StartupFailure {
        let (code, stage, requested) = match self {
            Self::WithRequest { request, source } => {
                let mut diagnostic = source.diagnostic();
                diagnostic.requested = Some(StartupTuple::from(*request));
                return diagnostic;
            }
            Self::CaptureDiscovery { request, .. } => {
                ("CaptureDiscovery", "Discovery", Some(*request))
            }
            Self::CaptureSelection { request, .. } => {
                ("CaptureSelection", "Discovery", Some(*request))
            }
            Self::CaptureArguments(_) => ("CaptureArguments", "Arguments", None),
            Self::Audio(_) => ("AudioUnavailable", "Discovery", None),
            Self::NoDisplay => ("NoDisplay", "NativeStartup", None),
            Self::InvalidDisplayEnvironment(_) => {
                ("InvalidDisplayEnvironment", "NativeStartup", None)
            }
            Self::MediaPrefixEnvironment(_) => ("MediaPrefixEnvironment", "MediaStartup", None),
            Self::InvalidMediaPrefix => ("InvalidMediaPrefix", "MediaStartup", None),
            Self::NativeLaunch(error) => (
                match error.failure_kind {
                    furami::native_host::NativeFailureKind::QtPluginMissing => "qt_plugin_missing",
                    furami::native_host::NativeFailureKind::QtPluginInvalid => "qt_plugin_invalid",
                    furami::native_host::NativeFailureKind::QtPluginLoad => "qt_plugin_load",
                    furami::native_host::NativeFailureKind::Qualification => "qualification",
                    _ => "NativeLaunch",
                },
                "NativeStartup",
                None,
            ),
            Self::NoFallbackExec(_) => ("NoFallbackExec", "Reexec", None),
            Self::QualificationArguments => ("qualification_arguments", "Arguments", None),
            Self::QualificationVideo(
                furami::media::qualification::QualificationVideoError::Path,
            ) => ("qualification_path", "Arguments", None),
            Self::QualificationVideo(
                furami::media::qualification::QualificationVideoError::Format,
            ) => ("qualification_format", "Arguments", None),
            Self::QualificationRenderer => ("qualification_renderer", "MediaStartup", None),
        };
        StartupFailure {
            code,
            stage,
            requested: requested.map(StartupTuple::from),
        }
    }
}

#[derive(Debug)]
struct StartupArguments {
    video: Option<CaptureArguments>,
    audio_source: Option<String>,
    audio_off: bool,
    list_audio_sources: bool,
    qualification_stdin: bool,
    qualification_video: Option<std::path::PathBuf>,
}

impl StartupArguments {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, StartupError> {
        let args: Vec<_> = args.into_iter().collect();
        let qualification = args
            .iter()
            .any(|argument| argument == "--qualification-video");
        Self::parse_inner(args).map_err(|error| {
            if qualification {
                StartupError::QualificationArguments
            } else {
                error
            }
        })
    }
    fn parse_inner(args: impl IntoIterator<Item = OsString>) -> Result<Self, StartupError> {
        let mut args = args.into_iter();
        let mut video_args = Vec::new();
        let mut audio_source = None;
        let (mut audio_off, mut list_audio_sources, mut qualification_stdin) =
            (false, false, false);
        let mut qualification_video = None;
        while let Some(flag) = args.next() {
            let text = flag.to_str().ok_or_else(|| {
                CaptureArgumentError::InvalidArgument("flag must be UTF-8".into())
            })?;
            match text {
                "--qualification-video" => {
                    if qualification_video.is_some() {
                        return Err(StartupError::QualificationArguments);
                    }
                    let path = args
                        .next()
                        .filter(|value| !value.as_encoded_bytes().starts_with(b"--"))
                        .ok_or(StartupError::QualificationArguments)?;
                    qualification_video = Some(std::path::PathBuf::from(path));
                }
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
        if qualification_video.is_some()
            && (!video_args.is_empty() || audio_source.is_some() || audio_off || list_audio_sources)
        {
            return Err(StartupError::QualificationArguments);
        }
        Ok(Self {
            video: CaptureArguments::parse(video_args)?,
            audio_off,
            audio_source,
            list_audio_sources,
            qualification_stdin,
            qualification_video,
        })
    }
    fn capture_audio(
        &self,
        saved: Option<&DraftSettings>,
        sources: &[audio::AudioSource],
    ) -> Result<(AudioSelection, furami::app::settings::StartupAudioPolicy), AudioError> {
        let selection = match self.audio_source.as_deref() {
            Some(name) => {
                let source = audio::select(sources, name)?;
                if self.audio_off {
                    AudioSelection::Disabled {
                        retained: Some(source),
                    }
                } else {
                    AudioSelection::Enabled { source }
                }
            }
            None if !self.audio_off => saved
                .map(|settings| settings.audio.clone())
                .unwrap_or_default(),
            None => AudioSelection::Disabled {
                retained: saved.and_then(|settings| settings.audio.source().cloned()),
            },
        };
        let policy = if self.audio_off || self.audio_source.is_some() {
            furami::app::settings::StartupAudioPolicy::Cli
        } else if saved.is_some() {
            furami::app::settings::StartupAudioPolicy::Explicit
        } else {
            furami::app::settings::StartupAudioPolicy::AutoCard
        };
        Ok((selection, policy))
    }
}

fn run(
    arguments: StartupArguments,
    diagnostics: DiagnosticsHandle,
    persistence: PersistenceSession,
) -> Result<(), StartupError> {
    let request = arguments.video.as_ref().map(|video| video.mode);
    run_with_arguments(arguments, diagnostics, persistence).map_err(|source| match request {
        Some(request) => StartupError::WithRequest {
            request,
            source: Box::new(source),
        },
        None => source,
    })
}

fn run_qualification(
    arguments: StartupArguments,
    diagnostics: DiagnosticsHandle,
) -> Result<(), StartupError> {
    let path = arguments
        .qualification_video
        .as_ref()
        .ok_or(StartupError::QualificationArguments)?;
    let video = furami::media::qualification::QualificationVideo::open(path)?;
    let display = match std::env::var("DISPLAY") {
        Ok(value) if !value.is_empty() => value,
        Ok(_) | Err(std::env::VarError::NotPresent) => return Err(StartupError::NoDisplay),
        Err(error) => return Err(StartupError::InvalidDisplayEnvironment(error)),
    };
    let prefix =
        std::env::var("FURAMI_MEDIA_PREFIX").map_err(StartupError::MediaPrefixEnvironment)?;
    let prefix_path = std::path::Path::new(&prefix);
    if !prefix_path.is_absolute() || !prefix_path.is_dir() {
        return Err(StartupError::InvalidMediaPrefix);
    }
    let capabilities = furami::media::render_catalog::query_qualified_capabilities(prefix_path)
        .map_err(|_| StartupError::QualificationRenderer)?;
    let renderer = furami::media::render_catalog::compile_renderer(
        &furami::domain::render::RendererSettings::default(),
        &capabilities,
    )
    .map_err(|_| StartupError::QualificationRenderer)?;
    furami::native_host::run_qualification_application(
        &prefix,
        &display,
        video,
        renderer,
        arguments.qualification_stdin,
        diagnostics,
    )?;
    Ok(())
}

fn run_with_arguments(
    arguments: StartupArguments,
    diagnostics: DiagnosticsHandle,
    mut persistence: PersistenceSession,
) -> Result<(), StartupError> {
    if arguments.list_audio_sources {
        for source in audio::discover()? {
            println!("{}\t{}", source.identity.name(), source.description);
        }
        return Ok(());
    }
    let mut saved = persistence.saved_selection();
    let restore_audio = !arguments.audio_off
        && arguments.audio_source.is_none()
        && arguments.video.is_none()
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
                tracing::warn!(event_code = "startup_audio_catalog_unavailable");
                Vec::new()
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        Vec::new()
    };
    let (audio, audio_policy) = arguments.capture_audio(saved.as_ref(), &audio_sources)?;
    if (arguments.audio_off || arguments.audio_source.is_some())
        && let Some(saved) = &mut saved
    {
        saved.audio = audio.clone();
    }
    let mut startup_binding = None;
    let mut topology_pin = None;
    let capture = arguments
        .video
        .map(|args| {
            let snapshot = linux::discover().map_err(|source| StartupError::CaptureDiscovery {
                request: args.mode,
                source,
            })?;
            let video = CaptureSelection::request_from_snapshot(&snapshot, &args.node, args.mode)
                .map_err(|source| StartupError::CaptureSelection {
                request: args.mode,
                source,
            })?;
            persistence.observe_profile_identities(
                snapshot.devices().iter().map(|device| device.identity()),
            );
            let identity = &video.identity;
            let serial_unique = identity.serial().is_some()
                && snapshot
                    .devices()
                    .iter()
                    .filter(|device| {
                        let observed = device.identity();
                        observed.vendor_id() == identity.vendor_id()
                            && observed.product_id() == identity.product_id()
                            && observed.serial() == identity.serial()
                    })
                    .count()
                    == 1;
            startup_binding = Some(persistence.profile_binding(identity, serial_unique));
            Ok::<_, StartupError>(DraftSettings {
                video,
                audio: audio.clone(),
                renderer: furami::domain::render::RendererSettings::default(),
            })
        })
        .transpose()?;
    let sources: Vec<_> = audio_sources
        .into_iter()
        .map(|source| source.identity)
        .collect();
    let mut startup = decide_startup(
        saved,
        capture,
        |settings| {
            use furami::app::settings::StartupVideoError;
            let snapshot = linux::discover()
                .map_err(|error| StartupVideoError::Unavailable(error.to_string()))?;
            persistence.observe_profile_identities(
                snapshot.devices().iter().map(|device| device.identity()),
            );
            let index = furami::domain::capture::resolve_identity(
                &settings.video.identity,
                snapshot.devices().iter().map(|device| device.identity()),
            )
            .map_err(|error| StartupVideoError::Unavailable(error.to_string()))?;
            startup_binding = persistence.saved_binding();
            if persistence.saved_binding() == Some(furami::settings::ProfileBinding::Topology)
                && snapshot.devices()[index].identity() != &settings.video.identity
            {
                return Err(StartupVideoError::Unavailable(
                    "The exact topology-bound last source is absent.".into(),
                ));
            }
            let stamp = WatchStamp {
                watch: WatchId::new(1).expect("nonzero initial preflight watch"),
                epoch: ObservationEpoch::new(1).expect("nonzero initial preflight epoch"),
            };
            let prepared =
                furami::capture::validate_prepared(settings.clone(), &snapshot, &[], stamp)
                    .map_err(StartupVideoError::from_failure)?;
            if startup_binding == Some(furami::settings::ProfileBinding::Topology) {
                let node = snapshot.devices()[index]
                    .nodes()
                    .iter()
                    .find(|node| node.devnode() == prepared.input().node())
                    .expect("preflight proved this unique input route");
                topology_pin = Some((node.syspath().to_owned(), node.usb_syspath().to_owned()));
            }
            Ok(())
        },
        restore_audio_error.map_or_else(|| Ok(sources.as_slice()), Err),
    );
    startup.audio_policy = audio_policy;
    startup.source_binding = startup_binding;
    startup.topology_pin = if startup.auto_open {
        topology_pin
    } else {
        None
    };
    if startup.draft.is_none() {
        startup.bootstrap_audio = audio;
    }
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
    // Query once, before Qt starts. Apply uses only this immutable typed result;
    // a query refusal remains visible without pretending capabilities exist.
    let treatment_capabilities =
        furami::media::render_catalog::query_qualified_capabilities(prefix);
    tracing::info!(event_code = "application_starting");
    furami::native_host::run_application(
        &media_prefix,
        treatment_capabilities,
        &x11_display,
        startup,
        persistence,
        sources,
        arguments.qualification_stdin,
        diagnostics,
    )?;
    tracing::info!(event_code = "application_exited");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_reports_reserve_unique_revisions_across_runtime_publications() {
        let runtime = DiagnosticsRuntime::start(DiagnosticsConfig {
            logging_enabled: false,
            xdg_state_home: None,
            home: None,
            limits: Default::default(),
        });
        let diagnostics = runtime.handle();
        let initial = initial_report(&diagnostics).unwrap();
        let runtime_revision = diagnostics.reserve_report_revision().unwrap();
        let failed = initial_report(&diagnostics).unwrap();
        assert!(initial.revision < runtime_revision);
        assert!(runtime_revision < failed.revision);
        runtime.begin_shutdown();
        runtime.finish_shutdown(std::time::Duration::from_millis(250));
    }

    #[test]
    fn invalid_cli_failure_is_classified_without_exposing_argument_content() {
        for argument in [
            "--private-startup-marker",
            "/home/private-startup-marker\nuntrusted argument text",
        ] {
            let error = StartupArguments::parse(args(&[argument])).unwrap_err();
            let failure = error.diagnostic();
            assert_eq!(failure.code, "CaptureArguments");
            assert_eq!(failure.stage, "Arguments");
            assert!(failure.requested.is_none());
            let encoded = serde_json::to_string(&failure).unwrap();
            assert!(!encoded.contains("private-startup-marker"));
            assert!(!encoded.contains("untrusted argument text"));
        }
    }

    #[test]
    fn startup_failure_report_does_not_publish_request_path_or_error_text() {
        let error = StartupError::NativeLaunch(furami::native_host::NativeLaunchError {
            exit_code: 1,
            diagnostic: "/dev/private-startup-marker personal backend text".into(),
            failure_kind: furami::native_host::NativeFailureKind::Generic,
            safe_detail: String::new(),
        });
        let encoded = serde_json::to_string(&error.diagnostic()).unwrap();
        assert!(!encoded.contains("private-startup-marker"));
        assert!(!encoded.contains("/dev/"));
        assert!(encoded.contains("NativeLaunch"));
    }

    #[test]
    fn startup_native_failure_retains_only_requested_tuple() {
        let arguments = StartupArguments::parse(args(&[
            "--capture-node",
            "/dev/private-startup-marker",
            "--capture-size",
            "1920x1080",
            "--capture-rate",
            "60000/1001",
        ]))
        .unwrap();
        let error = StartupError::WithRequest {
            request: arguments.video.unwrap().mode,
            source: Box::new(StartupError::NoDisplay),
        };
        let report = error.diagnostic();
        let tuple = report.requested.unwrap();
        assert_eq!(report.code, "NoDisplay");
        assert_eq!((tuple.width, tuple.height), (1920, 1080));
        assert_eq!(
            (tuple.rate_numerator, tuple.rate_denominator),
            (60000, 1001)
        );
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(!encoded.contains("private-startup-marker"));
    }

    fn saved_with_audio(audio: AudioSelection) -> DraftSettings {
        use furami::domain::capture::{
            CaptureMode, CapturedFourCc, DeviceIdentity, FrameRate, FrameSize, ModeRequest,
            UsbTopology, VideoRequest,
        };
        DraftSettings {
            video: VideoRequest::manual(ModeRequest {
                identity: DeviceIdentity::new(
                    0x32ed,
                    0x3701,
                    UsbTopology::new(
                        "pci-test".into(),
                        vec![std::num::NonZeroU8::new(1).unwrap()],
                    )
                    .unwrap(),
                    Some("raw-usb-serial".into()),
                )
                .unwrap(),
                mode: CaptureMode {
                    captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
                    size: FrameSize::new(1920, 1080).unwrap(),
                    rate: FrameRate::new(60000, 1001).unwrap(),
                },
            }),
            audio,
            renderer: Default::default(),
        }
    }

    #[test]
    fn startup_audio_provenance_distinguishes_factory_saved_and_explicit_cli_off() {
        use furami::app::settings::StartupAudioPolicy;
        let source = AudioSourceIdentity::new(
            "card.input".into(),
            vec![("device.serial".into(), "decorated-raw-usb-serial".into())],
        )
        .unwrap();
        let rows = vec![audio::AudioSource {
            identity: source.clone(),
            description: "Card".into(),
        }];
        let defaults = StartupArguments::parse(args(&[])).unwrap();
        assert_eq!(
            defaults.capture_audio(None, &rows).unwrap(),
            (AudioSelection::default(), StartupAudioPolicy::AutoCard)
        );
        let enabled = saved_with_audio(AudioSelection::Enabled {
            source: source.clone(),
        });
        assert_eq!(
            defaults.capture_audio(Some(&enabled), &[]).unwrap(),
            (enabled.audio.clone(), StartupAudioPolicy::Explicit)
        );
        let off = StartupArguments::parse(args(&["--capture-audio-off"])).unwrap();
        let (disabled, policy) = off.capture_audio(Some(&enabled), &[]).unwrap();
        assert!(!disabled.enabled());
        assert_eq!(disabled.source(), Some(&source));
        assert_eq!(policy, StartupAudioPolicy::Cli);
        let saved_off = saved_with_audio(AudioSelection::Disabled { retained: None });
        assert_eq!(
            defaults.capture_audio(Some(&saved_off), &rows).unwrap(),
            (saved_off.audio.clone(), StartupAudioPolicy::Explicit)
        );
        let explicit =
            StartupArguments::parse(args(&["--capture-audio-source", "card.input"])).unwrap();
        assert_eq!(
            explicit.capture_audio(None, &rows).unwrap(),
            (AudioSelection::Enabled { source }, StartupAudioPolicy::Cli)
        );
    }
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
    fn capture_cli_defaults_to_auto_without_fourcc_and_accepts_explicit_auto() {
        for suffix in [vec![], vec!["--capture-fourcc", "auto"]] {
            let mut values = vec![
                "--capture-node",
                "/dev/video0",
                "--capture-size",
                "1920x1080",
                "--capture-rate",
                "60/1",
                "--capture-audio-off",
            ];
            values.extend(suffix);
            let parsed = StartupArguments::parse(args(&values))
                .unwrap()
                .video
                .unwrap();
            assert_eq!(
                parsed.mode.format_policy,
                furami::domain::capture::InputFormatPolicy::Auto
            );
            assert_eq!(parsed.mode.size.width(), 1920);
            assert_eq!(
                parsed.mode.rate,
                furami::domain::capture::FrameRate::new(60, 1).unwrap()
            );
        }
        for fourcc in ["NV12", "BGR3", "AUTO"] {
            let parsed = StartupArguments::parse(args(&[
                "--capture-node",
                "/dev/video0",
                "--capture-size",
                "1920x1080",
                "--capture-rate",
                "60000/1001",
                "--capture-fourcc",
                fourcc,
            ]))
            .unwrap()
            .video
            .unwrap();
            assert_eq!(
                parsed.mode.format_policy,
                furami::domain::capture::InputFormatPolicy::Manual {
                    fourcc: furami::domain::capture::CapturedFourCc::from_bytes(
                        fourcc.as_bytes().try_into().unwrap()
                    ),
                }
            );
        }
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
        assert!(defaults.audio_source.is_none() && !defaults.audio_off);
        assert!(!defaults.list_audio_sources && !defaults.qualification_stdin);
        let explicit_off = StartupArguments::parse(args(&["--capture-audio-off"])).unwrap();
        assert!(explicit_off.audio_off && explicit_off.audio_source.is_none());
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
    #[test]
    fn qualification_arguments_are_exclusive_and_keep_unix_path_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(b"/tmp/qualification-\xff.y4m".to_vec());
        let parsed = StartupArguments::parse([
            OsString::from("--qualification-video"),
            path.clone(),
            OsString::from("--qualification-stdin"),
        ])
        .unwrap();
        assert_eq!(parsed.qualification_video.unwrap().as_os_str(), path);
        assert!(parsed.video.is_none() && parsed.qualification_stdin);
        for values in [
            vec!["--qualification-video"],
            vec![
                "--qualification-video",
                "/tmp/a.y4m",
                "--qualification-video",
                "/tmp/b.y4m",
            ],
            vec!["--qualification-video", "/tmp/a.y4m", "--capture-audio-off"],
            vec![
                "--qualification-video",
                "/tmp/a.y4m",
                "--list-audio-sources",
            ],
            vec![
                "--qualification-video",
                "/tmp/a.y4m",
                "--capture-node",
                "/dev/video0",
            ],
            vec![
                "--qualification-video",
                "/tmp/a.y4m",
                "--qualification-stdin",
                "--qualification-stdin",
            ],
        ] {
            let failure = StartupArguments::parse(args(&values))
                .unwrap_err()
                .diagnostic();
            assert_eq!(failure.code, "qualification_arguments", "{values:?}");
        }
    }
    #[test]
    fn typed_plugin_failure_survives_capture_request_wrapper_without_exposing_other_errors() {
        use furami::native_host::{NativeFailureKind, NativeLaunchError};
        for (kind, code) in [
            (NativeFailureKind::QtPluginMissing, "qt_plugin_missing"),
            (NativeFailureKind::QtPluginInvalid, "qt_plugin_invalid"),
            (NativeFailureKind::QtPluginLoad, "qt_plugin_load"),
        ] {
            let arguments = StartupArguments::parse(args(&[
                "--capture-node",
                "/dev/video0",
                "--capture-size",
                "320x180",
                "--capture-rate",
                "30",
            ]))
            .unwrap();
            let error = StartupError::WithRequest {
                request: arguments.video.unwrap().mode,
                source: Box::new(StartupError::NativeLaunch(NativeLaunchError {
                    exit_code: 1,
                    diagnostic: "private backend text".into(),
                    failure_kind: kind,
                    safe_detail: "platform: <media-prefix> plugin refused".into(),
                })),
            };
            assert_eq!(error.diagnostic().code, code);
            assert_eq!(
                error.plugin_detail(),
                Some("platform: <media-prefix> plugin refused")
            );
        }
        let generic = StartupError::NativeLaunch(NativeLaunchError {
            exit_code: 1,
            diagnostic: "private text".into(),
            failure_kind: NativeFailureKind::Generic,
            safe_detail: "must not be printed".into(),
        });
        assert!(generic.plugin_detail().is_none());
    }
}
