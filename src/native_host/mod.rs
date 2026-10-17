//! Responsibility: Small Qt/C++ bridge for the native parent and its events.
//! Allowed dependencies: Qt, typed XID/generation/intent exchanges.
pub use crate::ui::bridge::ffi::NativeFailureKind;

#[derive(Debug, thiserror::Error)]
#[error("Qt launch failed with status {exit_code}: {diagnostic}")]
pub struct NativeLaunchError {
    pub exit_code: i32,
    pub diagnostic: String,
    pub failure_kind: NativeFailureKind,
    pub safe_detail: String,
}

// Local exception: the Qt launch bootstrap keeps its eight values positional in
// one call, preserving the existing ownership handoff into RuntimeCoordinator.
#[allow(
    clippy::too_many_arguments,
    reason = "Qt/CXX positional boundary; one bootstrap call."
)]
pub fn run_application(
    media_prefix: &str,
    treatment_capabilities: Result<
        crate::media::render_catalog::RendererCapabilities,
        crate::media::render_catalog::RendererCatalogError,
    >,
    display: &str,
    startup: crate::app::settings::StartupSelection,
    persistence: crate::app::settings::PersistenceSession,
    sources: Vec<crate::domain::capture::AudioSourceIdentity>,
    qualification_stdin: bool,
    diagnostics: crate::diagnostics::DiagnosticsHandle,
) -> Result<(), NativeLaunchError> {
    let gate = Box::new(crate::ui::runtime::RuntimeCoordinator::new(
        media_prefix.to_owned(),
        treatment_capabilities,
        startup,
        persistence,
        sources,
        diagnostics,
    ));
    let result = crate::ui::bridge::ffi::run_qt_application(
        gate,
        display,
        media_prefix,
        qualification_stdin,
        false,
    );
    if result.exit_code == 0 {
        Ok(())
    } else {
        Err(NativeLaunchError {
            exit_code: result.exit_code,
            diagnostic: result.diagnostic,
            failure_kind: result.failure_kind,
            safe_detail: result.safe_detail,
        })
    }
}

pub fn run_qualification_application(
    media_prefix: &str,
    display: &str,
    video: crate::media::qualification::QualificationVideo,
    renderer: crate::media::render_catalog::CompiledRendererSettings,
    qualification_stdin: bool,
    diagnostics: crate::diagnostics::DiagnosticsHandle,
) -> Result<(), NativeLaunchError> {
    let runtime = crate::ui::runtime::RuntimeCoordinator::new_qualification(
        media_prefix.to_owned(),
        video,
        renderer,
        diagnostics,
    );
    let result = crate::ui::bridge::ffi::run_qt_application(
        Box::new(runtime),
        display,
        media_prefix,
        qualification_stdin,
        true,
    );
    if result.exit_code == 0 {
        Ok(())
    } else {
        Err(NativeLaunchError {
            exit_code: result.exit_code,
            diagnostic: result.diagnostic,
            failure_kind: result.failure_kind,
            safe_detail: result.safe_detail,
        })
    }
}

pub(crate) fn display_check(
    platform: &str,
    captured: &str,
    qt_display: &str,
    current: &str,
) -> String {
    if captured.is_empty() {
        return "missing X11 display: DISPLAY must be nonempty".to_owned();
    }
    if platform != "xcb" {
        return format!("Qt platform mismatch: {platform:?}; xcb required");
    }
    if captured != qt_display {
        return format!(
            "display mismatch: captured DISPLAY={captured:?}, Qt display={qt_display:?}"
        );
    }
    if captured != current {
        return format!(
            "DISPLAY changed after Qt initialization: captured={captured:?}, current={current:?}"
        );
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::display_check;

    #[test]
    fn display_mismatch_rejected_before_hosting() {
        let diagnostic = display_check("xcb", ":0", ":1", ":0");
        assert!(diagnostic.contains("display mismatch"));
    }

    #[test]
    fn platform_environment_and_empty_display_are_rejected() {
        assert!(!display_check("wayland", ":0", ":0", ":0").is_empty());
        assert!(!display_check("xcb", ":0", ":0", ":1").is_empty());
        assert!(!display_check("xcb", "", "", "").is_empty());
        assert!(display_check("xcb", ":0", ":0", ":0").is_empty());
    }
}
