#[cfg(not(any(cxxqt_qt_version_at_least_6_8, cxxqt_qt_version_at_least_7)))]
compile_error!("Furami requires Qt 6.8 or newer");

// Keep the library's generated QML registration linked into the executable.
extern crate furami as _;

mod app;
mod capture;
mod diagnostics;
mod domain;
mod native_host;
mod profiles;
mod settings;
mod ui;

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QUrl};
use tracing_subscriber::EnvFilter;

const MAIN_QML_URL: &str = "qrc:/qt/qml/dev/antho/furami/qml/Main.qml";

#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error("RUST_LOG must be valid UTF-8")]
    InvalidLogEnvironment(#[source] std::env::VarError),
    #[error("invalid RUST_LOG filter: {0}")]
    InvalidLogFilter(#[source] tracing_subscriber::filter::ParseError),
    #[error("failed to install tracing subscriber: {0}")]
    SubscriberInstallation(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("no desktop display: neither DISPLAY nor WAYLAND_DISPLAY is set")]
    NoDisplay,
    #[error("could not create Qt GUI application")]
    ApplicationConstruction,
    #[error("could not create QML application engine")]
    EngineConstruction,
    #[error("failed to create QML root object from embedded Main.qml")]
    QmlCreationFailed,
    #[error("QML root object creation is still pending after loading embedded Main.qml")]
    QmlCreationPending,
    #[error("Qt event loop exited with status {0}")]
    QtEventLoop(i32),
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
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(StartupError::NoDisplay);
    }

    tracing::info!(qml_url = MAIN_QML_URL, "starting Qt application");
    let mut app = QGuiApplication::new();
    app.as_mut().ok_or(StartupError::ApplicationConstruction)?;

    let mut engine = QQmlApplicationEngine::new();
    let creation_status = Arc::new(AtomicU8::new(0));
    {
        let mut engine_ref = engine.as_mut().ok_or(StartupError::EngineConstruction)?;
        let status_for_signal = Arc::clone(&creation_status);
        let _connection = engine_ref.as_mut().on_object_created(move |_, object, _| {
            status_for_signal.store(if object.is_null() { 1 } else { 2 }, Ordering::Release);
        });
        engine_ref.load(&QUrl::from(MAIN_QML_URL));

        match creation_status.load(Ordering::Acquire) {
            1 => return Err(StartupError::QmlCreationFailed),
            2 => {}
            _ => return Err(StartupError::QmlCreationPending),
        }
    }

    tracing::info!(qml_url = MAIN_QML_URL, "QML root object created");
    let exit_code = app
        .as_mut()
        .ok_or(StartupError::ApplicationConstruction)?
        .exec();
    drop(engine);
    drop(app);

    if exit_code != 0 {
        return Err(StartupError::QtEventLoop(exit_code));
    }
    tracing::info!("Qt application exited");
    Ok(())
}
