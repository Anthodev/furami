//! Explicit diagnostic projections. Never serialize application settings or UI snapshots here.
use serde::{Deserialize, Serialize};

use super::{DiagnosticsHealth, LogLimits};

mod audio;
mod budget;
mod emergency;
mod session;
pub(crate) use audio::AudioProjection;
pub(crate) use budget::{ProjectionBudget, ProjectionOversized};
pub use session::*;

#[cfg(test)]
mod session_tests;

fn serialize_inventory<S: serde::Serializer>(
    value: &std::sync::Arc<crate::diagnostics::inventory::BuildInventory>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    value.as_ref().serialize(serializer)
}

fn cached_inventory() -> std::sync::Arc<crate::diagnostics::inventory::BuildInventory> {
    static INVENTORY: std::sync::LazyLock<
        std::sync::Arc<crate::diagnostics::inventory::BuildInventory>,
    > = std::sync::LazyLock::new(|| {
        std::sync::Arc::new(crate::diagnostics::inventory::build_inventory())
    });
    INVENTORY.clone()
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReportRevision(pub u64);
impl ReportRevision {
    pub const INITIAL: Self = Self(1);
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Observation<T> {
    Observed { value: T },
    Unavailable { reason: UnavailableReason },
}
impl<T> Default for Observation<T> {
    fn default() -> Self {
        Self::Unavailable {
            reason: UnavailableReason::NotObserved,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum UnavailableReason {
    NotObserved,
    NotMeasured,
    NotExposed,
    NotInitialized,
    UnknownModel,
    Missing,
    AccessDenied,
    InvalidData,
    Oversized,
    BackendUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OsReport {
    pub target: String,
    pub distribution_id: Observation<String>,
    pub distribution_version: Observation<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DesktopSessionReport {
    pub session_type: Observation<String>,
    pub current_desktop: Observation<String>,
    pub wayland_display_present: bool,
}

impl DesktopSessionReport {
    pub fn from_environment() -> Self {
        fn variable(name: &str) -> Observation<String> {
            match std::env::var(name) {
                Ok(value) => Observation::Observed {
                    value: bounded_text(&value, 256),
                },
                Err(std::env::VarError::NotPresent) => Observation::Unavailable {
                    reason: UnavailableReason::Missing,
                },
                Err(std::env::VarError::NotUnicode(_)) => Observation::Unavailable {
                    reason: UnavailableReason::InvalidData,
                },
            }
        }
        Self {
            session_type: variable("XDG_SESSION_TYPE"),
            current_desktop: variable("XDG_CURRENT_DESKTOP"),
            wayland_display_present: std::env::var_os("WAYLAND_DISPLAY").is_some(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct NativeEnvironmentReport {
    pub qt_runtime_version: Observation<String>,
    pub qt_platform: Observation<String>,
    pub x_server: Observation<XServerReport>,
    pub xwayland_version: Observation<String>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct XServerReport {
    pub vendor: String,
    pub release: u32,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub shape_major: u32,
    pub shape_minor: u32,
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct RuntimeTreatmentCapabilities {
    pub renderer_available: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EnvironmentReport {
    pub furami_version: &'static str,
    pub os: Observation<OsReport>,
    pub desktop_session: DesktopSessionReport,
    pub log_location: DiagnosticsHealth,
    /// Embedded build/reference provenance. Present from the initial report,
    /// before any discovery, so the environment never omits it.
    #[serde(serialize_with = "serialize_inventory")]
    pub build_inventory: std::sync::Arc<crate::diagnostics::inventory::BuildInventory>,
    pub native: NativeEnvironmentReport,
    pub runtime_media: RuntimeMediaReport,
    pub runtime_treatment_capabilities: Observation<RuntimeTreatmentCapabilities>,
}
impl Default for EnvironmentReport {
    fn default() -> Self {
        Self {
            furami_version: env!("CARGO_PKG_VERSION"),
            os: Observation::default(),
            desktop_session: DesktopSessionReport::from_environment(),
            log_location: DiagnosticsHealth::starting(LogLimits::default()),
            build_inventory: cached_inventory(),
            native: NativeEnvironmentReport::default(),
            runtime_media: RuntimeMediaReport::default(),
            runtime_treatment_capabilities: Observation::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub operation: Option<u64>,
    pub attempt: Option<u64>,
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionReport {
    pub identity: SessionIdentity,
    pub phase: String,
    pub startup_failure: Option<StartupFailure>,
    pub gate_phase: &'static str,
    pub opening: Option<OpenKey>,
    pub selected_capture: Observation<CaptureTuple>,
    pub applied_capture: Observation<CaptureTuple>,
    pub treatments: TreatmentReport,
    pub audio: AudioReport,
    pub timing: TimingReport,
    pub failures: FailureLanes,
    pub applied_profile: Option<AppliedProfile>,
    #[serde(skip)]
    pub(crate) projection_oversized: bool,
}
impl Default for SessionReport {
    fn default() -> Self {
        Self {
            identity: SessionIdentity::default(),
            phase: "Starting".into(),
            startup_failure: None,
            gate_phase: "Idle",
            opening: None,
            selected_capture: Observation::default(),
            applied_capture: Observation::default(),
            treatments: TreatmentReport::default(),
            audio: AudioReport::default(),
            timing: TimingReport::default(),
            failures: FailureLanes::default(),
            applied_profile: None,
            projection_oversized: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ReportSnapshot {
    pub revision: ReportRevision,
    pub environment: EnvironmentReport,
    pub session: SessionReport,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StartupFailure {
    pub code: &'static str,
    pub stage: &'static str,
    pub requested: Option<StartupTuple>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct StartupTuple {
    pub format_policy: &'static str,
    pub fourcc: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub rate_numerator: u32,
    pub rate_denominator: u32,
}
impl From<crate::domain::capture::CaptureModeRequest> for StartupTuple {
    fn from(request: crate::domain::capture::CaptureModeRequest) -> Self {
        let (format_policy, fourcc) = match request.format_policy {
            crate::domain::capture::InputFormatPolicy::Auto => ("Auto", None),
            crate::domain::capture::InputFormatPolicy::Manual { fourcc } => {
                ("Manual", Some(fourcc.kernel_value()))
            }
        };
        Self {
            format_policy,
            fourcc,
            width: request.size.width(),
            height: request.size.height(),
            rate_numerator: request.rate.numerator(),
            rate_denominator: request.rate.denominator(),
        }
    }
}

/// Fallback deliberately does not claim that the full report was retained.
/// Every textual token is projected again here so fallback size is independent
/// of the rejected report, including reports constructed by injected callers.
#[derive(Serialize)]
pub(crate) struct EmergencyReport {
    pub incomplete: bool,
    pub reason: &'static str,
    pub identity: SessionIdentity,
    pub phase: &'static str,
    pub startup_failure: Option<StartupFailure>,
    #[serde(skip_serializing_if = "emergency::unavailable")]
    pub selected_capture: Observation<CaptureTuple>,
    #[serde(skip_serializing_if = "emergency::unavailable")]
    pub applied_capture: Observation<CaptureTuple>,
    #[serde(skip_serializing_if = "emergency::EmergencyFailures::is_empty")]
    pub failures: emergency::EmergencyFailures,
}
impl ReportSnapshot {
    pub fn initial() -> Self {
        Self {
            revision: ReportRevision::INITIAL,
            ..Self::default()
        }
    }
    pub(crate) fn emergency(&self) -> EmergencyReport {
        EmergencyReport {
            incomplete: true,
            reason: "OversizedReport",
            identity: self.session.identity,
            phase: emergency_phase(&self.session.phase),
            selected_capture: emergency::capture(&self.session.selected_capture),
            applied_capture: emergency::capture(&self.session.applied_capture),
            failures: (&self.session.failures).into(),
            startup_failure: self
                .session
                .startup_failure
                .as_ref()
                .map(|failure| StartupFailure {
                    code: match failure.code {
                        "CaptureDiscovery" => "CaptureDiscovery",
                        "CaptureSelection" => "CaptureSelection",
                        "CaptureArguments" => "CaptureArguments",
                        "AudioUnavailable" => "AudioUnavailable",
                        "NoDisplay" => "NoDisplay",
                        "InvalidDisplayEnvironment" => "InvalidDisplayEnvironment",
                        "MediaPrefixEnvironment" => "MediaPrefixEnvironment",
                        "InvalidMediaPrefix" => "InvalidMediaPrefix",
                        "NativeLaunch" => "NativeLaunch",
                        "NoFallbackExec" => "NoFallbackExec",
                        _ => "Unknown",
                    },
                    stage: match failure.stage {
                        "Discovery" => "Discovery",
                        "Arguments" => "Arguments",
                        "NativeStartup" => "NativeStartup",
                        "MediaStartup" => "MediaStartup",
                        "Reexec" => "Reexec",
                        _ => "Unknown",
                    },
                    requested: failure.requested.map(|mut requested| {
                        requested.format_policy = match requested.format_policy {
                            "Auto" => "Auto",
                            "Manual" => "Manual",
                            _ => "Unknown",
                        };
                        requested
                    }),
                }),
        }
    }
}

fn emergency_phase(phase: &str) -> &'static str {
    match phase {
        "Starting" => "Starting",
        "StartupFailed" => "StartupFailed",
        "Live" => "Live",
        "Stopped" => "Stopped",
        "Active" => "Active",
        "ApplyingTreatments" => "ApplyingTreatments",
        "RestoringTreatments" => "RestoringTreatments",
        "PausePending" => "PausePending",
        "Paused" => "Paused",
        "Validating" => "Validating",
        "ClosingOld" => "ClosingOld",
        "OpeningCandidate" => "OpeningCandidate",
        "CleaningFailedCandidate" => "CleaningFailedCandidate",
        "ValidatingPrior" => "ValidatingPrior",
        "OpeningRestore" => "OpeningRestore",
        "CleaningFailedRestore" => "CleaningFailedRestore",
        "ValidatingResume" => "ValidatingResume",
        "ClosingResume" => "ClosingResume",
        "OpeningResume" => "OpeningResume",
        "CleaningFailedResume" => "CleaningFailedResume",
        "ErrorWithActiveRestored" => "ErrorWithActiveRestored",
        "ErrorWithoutActive" => "ErrorWithoutActive",
        "Disconnected" => "Disconnected",
        "Recovering" => "Recovering",
        "SelectionRequired" => "SelectionRequired",
        "Stopping" => "Stopping",
        "ShutdownReady" => "ShutdownReady",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_report_has_no_invented_session_or_os_observation() {
        let report = ReportSnapshot::initial();
        assert_eq!(report.revision, ReportRevision::INITIAL);
        assert_eq!(report.session.phase, "Starting");
        assert_eq!(report.session.identity, SessionIdentity::default());
        assert!(report.session.startup_failure.is_none());
        assert!(matches!(
            report.environment.os,
            Observation::Unavailable { .. }
        ));
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(!encoded.contains("/dev/"));
    }

    #[test]
    fn emergency_is_bounded_and_retains_current_startup_context() {
        let mut report = ReportSnapshot::initial();
        report.session.identity = SessionIdentity {
            operation: Some(9),
            attempt: Some(11),
            generation: Some(13),
        };
        report.session.phase = "private-phase-marker".repeat(100_000);
        report.session.startup_failure = Some(StartupFailure {
            code: "CaptureSelection",
            stage: "Discovery",
            requested: Some(StartupTuple {
                format_policy: "Manual",
                fourcc: Some(0x3231564e),
                width: 1920,
                height: 1080,
                rate_numerator: 60000,
                rate_denominator: 1001,
            }),
        });
        let projected = report.emergency();
        let bytes = serde_json::to_vec(&projected).unwrap();
        assert!(bytes.len() < 1024);
        assert_eq!(projected.identity, report.session.identity);
        assert_eq!(projected.phase, "Unknown");
        let failure = projected.startup_failure.unwrap();
        assert_eq!(failure.code, "CaptureSelection");
        assert_eq!(failure.requested.unwrap().rate_denominator, 1001);
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains("private-phase-marker")
        );
    }

    #[test]
    fn emergency_rejects_unrecognized_static_tokens() {
        let mut report = ReportSnapshot::initial();
        report.session.phase = "StartupFailed".into();
        report.session.startup_failure = Some(StartupFailure {
            code: "/private/code",
            stage: "/private/stage",
            requested: Some(StartupTuple {
                format_policy: "/private/policy",
                fourcc: None,
                width: 1,
                height: 1,
                rate_numerator: 1,
                rate_denominator: 1,
            }),
        });
        let projected = report.emergency();
        assert_eq!(projected.phase, "StartupFailed");
        let bytes = serde_json::to_vec(&projected).unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains("/private/"));
    }
}
