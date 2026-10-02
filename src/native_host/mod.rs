//! Responsibility: Small Qt/C++ bridge for the native parent and its events.
//! Allowed dependencies: Qt, typed XID/generation/intent exchanges.

#[derive(Debug, thiserror::Error)]
#[error("Qt launch failed with status {exit_code}: {diagnostic}")]
pub struct NativeLaunchError {
    pub exit_code: i32,
    pub diagnostic: String,
}

pub fn run_application(
    media_prefix: &str,
    display: &str,
    selection: Option<crate::capture::input::CaptureSelection>,
) -> Result<(), NativeLaunchError> {
    let gate = Box::new(crate::app::gate::GateCoordinator::new(
        media_prefix.to_owned(),
        selection,
    ));
    let result = crate::ui::bridge::ffi::run_qt_application(gate, display);
    if result.exit_code == 0 {
        Ok(())
    } else {
        Err(NativeLaunchError {
            exit_code: result.exit_code,
            diagnostic: result.diagnostic,
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
