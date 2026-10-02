#[cfg(not(any(cxxqt_qt_version_at_least_6_8, cxxqt_qt_version_at_least_7)))]
compile_error!("Furami requires Qt 6.8 or newer");

// Keep the library's generated QML registration linked into the executable.
extern crate furami as _;

mod diagnostics;
mod profiles;
mod settings;
use furami::capture::{
    input::{CaptureArgumentError, CaptureArguments, CaptureSelection, SelectionError},
    linux,
};
use std::ffi::OsString;
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
}

fn main() -> anyhow::Result<()> {
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

fn run() -> Result<(), StartupError> {
    let capture = parse_capture_args(std::env::args_os().skip(1))?
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
            Ok::<_, StartupError>(selection)
        })
        .transpose()?;
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
    furami::native_host::run_application(&media_prefix, &x11_display, capture)?;
    tracing::info!("Qt application exited");
    Ok(())
}

fn parse_capture_args(
    args: impl IntoIterator<Item = OsString>,
) -> Result<Option<CaptureArguments>, StartupError> {
    Ok(CaptureArguments::parse(args)?)
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
        assert!(parse_capture_args(args(&[])).unwrap().is_none());
        let parsed = parse_capture_args(args(&[
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
        .unwrap();
        assert_eq!(parsed.node, std::path::Path::new("/dev/video0"));
        assert_eq!(parsed.mode.rate.numerator(), 60);
        assert_eq!(parsed.mode.rate.denominator(), 1);
        assert!(matches!(
            parse_capture_args(args(&["--capture-node", "/dev/video0"])),
            Err(StartupError::CaptureArguments(
                CaptureArgumentError::IncompleteSelection
            ))
        ));
        for value in ["0x1440", "2560x0", "2560x1440x1"] {
            assert!(
                parse_capture_args(args(&[
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
            parse_capture_args(args(&[
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
            assert!(parse_capture_args(args(&values)).is_err());
        }
    }
}
