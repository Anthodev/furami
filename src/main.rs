#[cfg(not(any(cxxqt_qt_version_at_least_6_8, cxxqt_qt_version_at_least_7)))]
compile_error!("Furami requires Qt 6.8 or newer");

// Keep the library's generated QML registration linked into the executable.
extern crate furami as _;

mod diagnostics;
mod profiles;
mod settings;
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
    furami::native_host::run_application(&media_prefix, &x11_display)?;
    tracing::info!("Qt application exited");
    Ok(())
}
