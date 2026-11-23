//! Exhaustive route dispatch: qualification never constructs capture services.
use super::qualification::QualificationRuntime;
use super::*;

#[allow(
    clippy::large_enum_variant,
    reason = "The two application routes retain direct ownership; capture fixtures use CaptureRuntime directly."
)]
pub(crate) enum RuntimeCoordinator {
    Capture(CaptureRuntime),
    Qualification(QualificationRuntime),
}
macro_rules! capture_intents {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {$(
        pub(crate) fn $name(&mut self, $($arg: $ty),*) -> UiUpdate {
            match self {
                Self::Capture(runtime) => runtime.$name($($arg),*),
                Self::Qualification(runtime) => { let _ = ($($arg,)*); runtime.reject(stringify!($name)) }
            }
        }
    )*};
}
macro_rules! shared_intents {
    ($($(#[$attr:meta])* $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {$(
        $(#[$attr])*
        pub(crate) fn $name(&mut self, $($arg: $ty),*) -> $ret {
            match self {
                Self::Capture(runtime) => runtime.$name($($arg),*),
                Self::Qualification(runtime) => runtime.$name($($arg),*),
            }
        }
    )*};
}
impl RuntimeCoordinator {
    pub(crate) fn new(
        prefix: String,
        capabilities: Result<
            crate::media::render_catalog::RendererCapabilities,
            crate::media::render_catalog::RendererCatalogError,
        >,
        startup: StartupSelection,
        persistence: PersistenceSession,
        diagnostics: crate::diagnostics::DiagnosticsHandle,
    ) -> Self {
        Self::Capture(CaptureRuntime::new(
            prefix,
            capabilities,
            startup,
            persistence,
            diagnostics,
        ))
    }
    pub(crate) fn new_qualification(
        prefix: String,
        video: crate::media::qualification::QualificationVideo,
        renderer: crate::media::render_catalog::CompiledRendererSettings,
        diagnostics: crate::diagnostics::DiagnosticsHandle,
    ) -> Self {
        Self::Qualification(QualificationRuntime::new(
            prefix,
            video,
            renderer,
            diagnostics,
        ))
    }
    capture_intents! {
        refresh_catalogs();
        select_capture(revision: &str, catalog: &str, row: &str);
        select_size(revision: &str, catalog: &str, row: &str);
        set_size(revision: &str, catalog: &str, width: &str, height: &str);
        select_rate(revision: &str, catalog: &str, row: &str);
        set_rate(revision: &str, catalog: &str, exact: &str);
        select_format(revision: &str, catalog: &str, row: &str);
        set_audio_enabled(revision: &str, enabled: bool);
        select_audio_input(revision: &str, catalog: &str, row: &str);
        retry_save(); close_without_save(); request_reset();
        decide_reset(token: u64, confirmed: bool);
        set_stats_hidden(hidden: bool); set_logging_enabled(enabled: bool);
        open(); restart(attempt: u64); reconnect(attempt: u64);
        choose_recovery(attempt: u64, watch: u64, epoch: u64, candidate: u64);
        open_log_folder();
        log_folder_result(request: u64, accepted: bool);
        log_folder_finished(request: u64, failed: bool);
    }
    shared_intents! {
        ui_ready() -> UiUpdate;
        request_application_close() -> UiUpdate;
        set_fullscreen(fullscreen: bool) -> UiUpdate;
        close(attempt: u64) -> UiUpdate;
        quit() -> UiUpdate;
        surface_ready(token: SurfaceToken) -> UiUpdate;
        surface_lost(attempt: AttemptId) -> UiUpdate;
        wait_for_owner_ack(attempt: AttemptId) -> String;
        native_released(attempt: AttemptId) -> UiUpdate;
        pause(attempt: AttemptId) -> SubmitStatus;
        poll() -> UiUpdate;
    }
    pub(crate) fn qualification_command(&mut self, command: QualificationCommand) -> UiUpdate {
        match self {
            Self::Qualification(runtime) => runtime.qualification_command(command),
            Self::Capture(runtime) => runtime.poll(),
        }
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn owner_ack_failed(&self) -> bool {
        match self {
            Self::Capture(runtime) => runtime.owner_ack_failed(),
            Self::Qualification(runtime) => runtime.owner_ack_failed(),
        }
    }
    pub(crate) fn set_volume(&mut self, percent: i32) -> SubmitStatus {
        match self {
            Self::Capture(runtime) => runtime.set_volume(percent),
            Self::Qualification(_) => SubmitStatus::NotReady,
        }
    }
    pub(crate) fn set_muted(&mut self, muted: bool) -> SubmitStatus {
        match self {
            Self::Capture(runtime) => runtime.set_muted(muted),
            Self::Qualification(_) => SubmitStatus::NotReady,
        }
    }
    pub(crate) fn select_output(&mut self, row: &str, revision: u64) -> SubmitStatus {
        match self {
            Self::Capture(runtime) => runtime.select_output(row, revision),
            Self::Qualification(_) => SubmitStatus::NotReady,
        }
    }
    pub(crate) fn claim_log_folder_dispatch(&mut self, request: u64) -> bool {
        match self {
            Self::Capture(runtime) => runtime.claim_log_folder_dispatch(request),
            Self::Qualification(_) => false,
        }
    }
    pub(crate) fn native_environment(
        &mut self,
        report: crate::diagnostics::report::NativeEnvironmentReport,
    ) -> UiUpdate {
        match self {
            Self::Capture(runtime) => runtime.native_environment(report),
            Self::Qualification(runtime) => runtime.native_environment(report),
        }
    }
}
