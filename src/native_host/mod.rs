//! GPUI and X11 native window ownership.

mod gpui;
pub use self::gpui::GpuiLaunchError;

pub fn run_gpui_application(
    media_prefix: &str,
    treatment_capabilities: Result<
        crate::media::render_catalog::RendererCapabilities,
        crate::media::render_catalog::RendererCatalogError,
    >,
    display: &str,
    startup: crate::app::settings::StartupSelection,
    persistence: crate::app::settings::PersistenceSession,
    diagnostics: crate::diagnostics::DiagnosticsHandle,
) -> Result<(), GpuiLaunchError> {
    let runtime = crate::ui::runtime::RuntimeCoordinator::new(
        media_prefix.to_owned(),
        treatment_capabilities,
        startup,
        persistence,
        diagnostics,
    );
    gpui::run(display, runtime, false)
}

pub fn run_gpui_qualification_application(
    media_prefix: &str,
    display: &str,
    video: crate::media::qualification::QualificationVideo,
    renderer: crate::media::render_catalog::CompiledRendererSettings,
    qualification_stdin: bool,
    diagnostics: crate::diagnostics::DiagnosticsHandle,
) -> Result<(), GpuiLaunchError> {
    let runtime = crate::ui::runtime::RuntimeCoordinator::new_qualification(
        media_prefix.to_owned(),
        video,
        renderer,
        diagnostics,
    );
    gpui::run(display, runtime, qualification_stdin)
}
