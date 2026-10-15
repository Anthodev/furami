//! Safe, explicit projections of session authorities. No locators or free-form errors.
use super::{Observation, SessionIdentity, StartupTuple, UnavailableReason};
use crate::domain::{capture::*, render::*, state::*};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ExpectedCardModel {
    GenkiShadowCast3,
}
impl ExpectedCardModel {
    pub fn label(self) -> &'static str {
        match self {
            Self::GenkiShadowCast3 => "GENKI ShadowCast 3",
        }
    }
}
pub fn expected_card_model(vendor_id: u16, product_id: u16) -> Option<ExpectedCardModel> {
    match (vendor_id, product_id) {
        (0x32ed, 0x3701) => Some(ExpectedCardModel::GenkiShadowCast3),
        _ => None,
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CaptureTuple {
    pub vendor_id: u16,
    pub product_id: u16,
    pub expected_card_model: Observation<ExpectedModel>,
    pub requested: StartupTuple,
    pub resolved_fourcc: Option<u32>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExpectedModel {
    pub code: ExpectedCardModel,
    pub label: &'static str,
}
impl CaptureTuple {
    pub(crate) fn new(
        vendor_id: u16,
        product_id: u16,
        mode: CaptureModeRequest,
        fourcc: Option<CapturedFourCc>,
    ) -> Self {
        Self {
            vendor_id,
            product_id,
            expected_card_model: expected_card_model(vendor_id, product_id)
                .map(|code| Observation::Observed {
                    value: ExpectedModel {
                        code,
                        label: code.label(),
                    },
                })
                .unwrap_or(Observation::Unavailable {
                    reason: UnavailableReason::UnknownModel,
                }),
            requested: mode.into(),
            resolved_fourcc: fourcc.map(CapturedFourCc::kernel_value),
        }
    }
}
impl From<&VideoRequest> for CaptureTuple {
    fn from(value: &VideoRequest) -> Self {
        Self::new(
            value.identity.vendor_id(),
            value.identity.product_id(),
            value.mode,
            value.resolved_fourcc,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RendererReport {
    pub scale: ScalerKernel,
    pub cscale: ScalerChoice,
    pub dscale: ScalerChoice,
    pub scale_antiring: f64,
    pub dither: DitherMode,
    pub deband_enabled: bool,
    pub deband_iterations: u8,
    pub deband_grain: f64,
    pub interpolation: bool,
}
impl From<&RendererSettings> for RendererReport {
    fn from(settings: &RendererSettings) -> Self {
        let v = settings.values();
        Self {
            scale: v.scale,
            cscale: v.cscale,
            dscale: v.dscale,
            scale_antiring: v.scale_antiring,
            dither: v.dither,
            deband_enabled: v.deband.enabled,
            deband_iterations: v.deband.iterations,
            deband_grain: v.deband.grain,
            interpolation: v.interpolation,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TreatmentReport {
    pub active_renderer: Observation<RendererReport>,
    pub requested_renderer: Option<RendererReport>,
    pub pending: Option<TreatmentKey>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TreatmentKey {
    pub operation: u64,
    pub attempt: u64,
    pub pass: TreatmentPass,
}
impl From<TreatmentAttemptKey> for TreatmentKey {
    fn from(v: TreatmentAttemptKey) -> Self {
        Self {
            operation: v.apply.get(),
            attempt: v.attempt.get(),
            pass: v.pass,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OpenKey {
    pub operation: u64,
    pub attempt: u64,
    pub purpose: AttemptPurpose,
}
impl From<AttemptKey> for OpenKey {
    fn from(v: AttemptKey) -> Self {
        Self {
            operation: v.apply.get(),
            attempt: v.attempt.get(),
            purpose: v.purpose,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum SelectedOutput {
    #[default]
    Auto,
    Manual {
        technical_name: String,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EffectiveRoute {
    pub source_technical_name: String,
    pub sink_technical_name: String,
    pub attempt: u64,
    pub generation: u64,
    pub audio_epoch: u64,
    pub output_revision: u64,
    pub watch: u64,
    pub observation_epoch: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum AudioCode {
    Disabled,
    WaitingForSource,
    PendingRoute,
    Paused,
    Failed,
    Opening,
    Active,
    Switching,
    Detaching,
    Blocked,
    NoAvailableOutput,
    CatalogUnavailable,
    ManualUnavailable,
    ManualRequiresAction,
    RoutingConflict,
}
pub(crate) fn output_silence(value: &crate::domain::output::OutputSilence) -> AudioCode {
    use crate::domain::output::OutputSilence::*;
    match value {
        NoAvailableOutput => AudioCode::NoAvailableOutput,
        CatalogUnavailable(_) => AudioCode::CatalogUnavailable,
        ManualUnavailable => AudioCode::ManualUnavailable,
        ManualRequiresAction => AudioCode::ManualRequiresAction,
        RoutingConflict(_) => AudioCode::RoutingConflict,
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct AudioReport {
    pub selected_output: SelectedOutput,
    pub effective_route: Observation<EffectiveRoute>,
    pub availability: Observation<AudioCode>,
    pub audio_epoch: Option<u64>,
    pub output_revision: Option<u64>,
    pub plan_output_revision: u64,
    pub plan_reason: Option<AudioCode>,
    pub helpers: HelperReport,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct HelperReport {
    pub pw_loopback: HelperStatus,
    pub pw_metadata: HelperStatus,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(tag = "status")]
pub enum HelperStatus {
    #[default]
    NotObserved,
    SpawnSucceeded,
    SpawnFailed {
        io_kind: crate::diagnostics::IoKind,
        os_code: Option<i32>,
    },
}
impl From<crate::media::loopback::HelperObservations> for HelperReport {
    fn from(value: crate::media::loopback::HelperObservations) -> Self {
        fn one(value: crate::media::loopback::HelperObservation) -> HelperStatus {
            use crate::media::loopback::HelperObservation::*;
            match value {
                NotObserved => HelperStatus::NotObserved,
                SpawnSucceeded => HelperStatus::SpawnSucceeded,
                SpawnFailed { io_kind, os_code } => HelperStatus::SpawnFailed {
                    io_kind: io_kind.into(),
                    os_code,
                },
            }
        }
        Self {
            pw_loopback: one(value.pw_loopback),
            pw_metadata: one(value.pw_metadata),
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RuntimeMediaReport {
    pub generation: Option<u64>,
    pub observation_epoch: Option<u64>,
    pub client_api: Observation<u64>,
    pub libmpv_version: Observation<String>,
    pub backend: Observation<BackendReport>,
    pub backend_unavailable: Option<&'static str>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackendReport {
    pub video_output: String,
    pub gpu_context: String,
}
impl From<&crate::media::controller::RuntimeMediaObservation> for RuntimeMediaReport {
    fn from(v: &crate::media::controller::RuntimeMediaObservation) -> Self {
        use crate::media::controller::BackendObservation;
        let (backend, backend_unavailable) = match &v.backend {
            BackendObservation::Observed {
                video_output,
                gpu_context,
            } => (
                Observation::Observed {
                    value: BackendReport {
                        video_output: bounded_text(video_output, 128),
                        gpu_context: bounded_text(gpu_context, 128),
                    },
                },
                None,
            ),
            BackendObservation::Unavailable(reason) => (
                Observation::Unavailable {
                    reason: UnavailableReason::BackendUnavailable,
                },
                Some(backend_reason(*reason)),
            ),
        };
        Self {
            generation: Some(v.generation),
            observation_epoch: Some(v.observation_epoch),
            client_api: Observation::Observed {
                value: v.client_api,
            },
            libmpv_version: Observation::Observed {
                value: bounded_text(&v.libmpv_version, 128),
            },
            backend,
            backend_unavailable,
        }
    }
}
fn backend_reason(reason: crate::media::controller::BackendUnavailable) -> &'static str {
    use crate::media::controller::BackendUnavailable::*;
    match reason {
        NotObserved => "NotObserved",
        Invalidated => "Invalidated",
        NotConfigured => "NotConfigured",
        PropertyUnavailable => "PropertyUnavailable",
        InvalidPayload => "InvalidPayload",
        SubmissionRejected => "SubmissionRejected",
        EpochExhausted => "EpochExhausted",
    }
}
impl RuntimeMediaReport {
    pub(crate) fn matches_observation(
        &self,
        observation: Option<&crate::media::controller::RuntimeMediaObservation>,
    ) -> bool {
        use crate::media::controller::BackendObservation;
        let Some(value) = observation else {
            return self == &Self::default();
        };
        let version = matches!(&self.libmpv_version, Observation::Observed { value: cached } if cached == bounded_str(&value.libmpv_version, 128));
        let backend = match (&self.backend, &value.backend) {
            (
                Observation::Observed { value: cached },
                BackendObservation::Observed {
                    video_output,
                    gpu_context,
                },
            ) => {
                cached.video_output == bounded_str(video_output, 128)
                    && cached.gpu_context == bounded_str(gpu_context, 128)
                    && self.backend_unavailable.is_none()
            }
            (
                Observation::Unavailable {
                    reason: UnavailableReason::BackendUnavailable,
                },
                BackendObservation::Unavailable(reason),
            ) => self.backend_unavailable == Some(backend_reason(*reason)),
            _ => false,
        };
        self.generation == Some(value.generation)
            && self.observation_epoch == Some(value.observation_epoch)
            && matches!(self.client_api, Observation::Observed { value: api } if api == value.client_api)
            && version
            && backend
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct RationalRate {
    pub numerator: u32,
    pub denominator: u32,
}
impl From<FrameRate> for RationalRate {
    fn from(v: FrameRate) -> Self {
        Self {
            numerator: v.numerator(),
            denominator: v.denominator(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlaybackTiming {
    pub value: f64,
    pub source: crate::media::session::Source,
    pub status: &'static str,
    pub scope: &'static str,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TimingReport {
    pub requested: Observation<RationalRate>,
    pub derived_configured_value: Observation<f64>,
    pub playback: Observation<PlaybackTiming>,
    pub measured_capture_cadence: Observation<f64>,
}
impl Default for TimingReport {
    fn default() -> Self {
        Self {
            requested: Observation::default(),
            derived_configured_value: Observation::default(),
            playback: Observation::default(),
            measured_capture_cadence: Observation::Unavailable {
                reason: UnavailableReason::NotMeasured,
            },
        }
    }
}
impl TimingReport {
    pub(crate) fn from_receipt(facts: &crate::media::gate::ReceiptTimingFacts) -> Self {
        use crate::{app::ports::FactStatus, media::session::Source};
        let mut result = Self {
            requested: Observation::Observed {
                value: facts.requested.into(),
            },
            derived_configured_value: Observation::Observed {
                value: f64::from(facts.requested.numerator())
                    / f64::from(facts.requested.denominator()),
            },
            ..Self::default()
        };
        if let Some(observation) = facts
            .observation
            .filter(|v| v.value.is_finite() && v.value > 0.0)
        {
            let status = match (facts.status, observation.source) {
                (FactStatus::Configured, Source::MpvConfiguredContainerFps) => Some("Configured"),
                (FactStatus::Approximate, Source::MpvContainerFps) => Some("Approximate"),
                _ => None,
            };
            if let Some(status) = status {
                result.playback = Observation::Observed {
                    value: PlaybackTiming {
                        value: observation.value,
                        source: observation.source,
                        status,
                        scope: if status == "Configured" {
                            "application-configured playback timing"
                        } else {
                            "nominal playback timing; not measured capture cadence"
                        },
                    },
                };
            }
        }
        result
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ProfileOrigin {
    Startup,
    SourceSelection,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ProfileBindingKind {
    Serial,
    Topology,
}
impl From<crate::settings::ProfileBinding> for ProfileBindingKind {
    fn from(v: crate::settings::ProfileBinding) -> Self {
        match v {
            crate::settings::ProfileBinding::Serial => Self::Serial,
            crate::settings::ProfileBinding::Topology => Self::Topology,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct AppliedProfile {
    pub binding_kind: ProfileBindingKind,
    pub origin: ProfileOrigin,
    pub matches_loaded_settings: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FailureLanes {
    pub validation: Option<FailureContext>,
    pub candidate: Option<FailureContext>,
    pub restore: Option<FailureContext>,
    pub resume: Option<FailureContext>,
    pub incumbent: Option<FailureContext>,
    pub cleanup: Vec<FailureContext>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FailureContext {
    pub identity: SessionIdentity,
    pub phase_at_failure: &'static str,
    pub correlation: FailureCorrelation,
    pub requested: CaptureTuple,
    pub category: crate::domain::failure::FailureCategory,
    pub stage: crate::domain::failure::Stage,
    pub cause: crate::domain::failure::Cause,
    pub operation: &'static str,
    pub evidence: Option<BackendProof>,
    pub treatment: Option<TreatmentFailureReport>,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BackendProof {
    pub operation: crate::domain::failure::BackendOperation,
    pub errno: i32,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum FailureCorrelation {
    Validation {
        operation: u64,
        purpose: AttemptPurpose,
        revision: u64,
        watch: u64,
        observation_epoch: u64,
    },
    Attempt {
        key: OpenKey,
    },
    Treatment {
        key: TreatmentKey,
    },
    Owner {
        attempt: u64,
        audio_epoch: Option<u64>,
    },
    Observation {
        watch: u64,
        observation_epoch: u64,
        validation: Option<ValidationCorrelation>,
        attempt: Option<u64>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ValidationCorrelation {
    pub operation: u64,
    pub purpose: AttemptPurpose,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TreatmentFailureReport {
    pub key: Option<TreatmentKey>,
    pub disposition: crate::domain::failure::TreatmentFailureDisposition,
    pub error: TreatmentErrorReport,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum TreatmentErrorReport {
    Renderer {
        property: RendererProperty,
        code: crate::domain::failure::RendererErrorKind,
    },
    Unconfirmed {
        reason: crate::domain::failure::TreatmentConfirmationFailure,
    },
}
impl From<&crate::app::diagnostic::DiagnosticFailureContexts> for FailureLanes {
    fn from(v: &crate::app::diagnostic::DiagnosticFailureContexts) -> Self {
        Self {
            validation: v.validation.as_deref().map(FailureContext::from),
            candidate: v.candidate.as_deref().map(FailureContext::from),
            restore: v.restore.as_deref().map(FailureContext::from),
            resume: v.resume.as_deref().map(FailureContext::from),
            incumbent: v.incumbent.as_deref().map(FailureContext::from),
            cleanup: v
                .cleanup
                .iter()
                .filter_map(|v| v.as_deref().map(FailureContext::from))
                .collect(),
        }
    }
}
impl From<&crate::app::diagnostic::DiagnosticFailureContext> for FailureContext {
    fn from(v: &crate::app::diagnostic::DiagnosticFailureContext) -> Self {
        use crate::app::diagnostic::{
            DiagnosticFailureCorrelation as C, DiagnosticTreatmentError as E,
        };
        let validation = |key: ValidationKey| ValidationCorrelation {
            operation: key.apply.get(),
            purpose: key.purpose,
        };
        let correlation = match v.correlation {
            C::Validation {
                key,
                revision,
                watch,
            } => FailureCorrelation::Validation {
                operation: key.apply.get(),
                purpose: key.purpose,
                revision: revision.get(),
                watch: watch.watch.get(),
                observation_epoch: watch.epoch.get(),
            },
            C::Attempt(key) => FailureCorrelation::Attempt { key: key.into() },
            C::Treatment(key) => FailureCorrelation::Treatment { key: key.into() },
            C::Owner {
                attempt,
                audio_epoch,
            } => FailureCorrelation::Owner {
                attempt: attempt.get(),
                audio_epoch: audio_epoch.map(AudioEpoch::get),
            },
            C::Observation {
                stamp,
                validation: key,
                attempt,
            } => FailureCorrelation::Observation {
                watch: stamp.watch.get(),
                observation_epoch: stamp.epoch.get(),
                validation: key.map(validation),
                attempt: attempt.map(AttemptId::get),
            },
        };
        let error = &v.error;
        Self {
            identity: SessionIdentity {
                operation: v.identity.operation().map(ApplyId::get),
                attempt: v.identity.attempt().map(AttemptId::get),
                generation: None,
            },
            phase_at_failure: phase_code(v.identity.phase()),
            correlation,
            requested: CaptureTuple::new(
                error.requested.vendor_id,
                error.requested.product_id,
                error.requested.mode,
                error.requested.resolved_fourcc,
            ),
            category: error.category,
            stage: error.stage,
            cause: error.cause,
            operation: error.operation,
            evidence: error.evidence.as_ref().map(|v| BackendProof {
                operation: v.operation,
                errno: v.errno,
            }),
            treatment: error.treatment.as_ref().map(|v| TreatmentFailureReport {
                key: v.key.map(TreatmentKey::from),
                disposition: v.disposition,
                error: match &v.kind {
                    E::Renderer { property, kind } => TreatmentErrorReport::Renderer {
                        property: *property,
                        code: kind.clone(),
                    },
                    E::Unconfirmed { reason } => {
                        TreatmentErrorReport::Unconfirmed { reason: *reason }
                    }
                },
            }),
        }
    }
}
pub(crate) fn phase_code(phase: ProductPhase) -> &'static str {
    match phase {
        ProductPhase::Stopped => "Stopped",
        ProductPhase::Active => "Active",
        ProductPhase::ApplyingTreatments => "ApplyingTreatments",
        ProductPhase::RestoringTreatments => "RestoringTreatments",
        ProductPhase::PausePending => "PausePending",
        ProductPhase::Paused => "Paused",
        ProductPhase::Validating => "Validating",
        ProductPhase::ClosingOld => "ClosingOld",
        ProductPhase::OpeningCandidate => "OpeningCandidate",
        ProductPhase::CleaningFailedCandidate => "CleaningFailedCandidate",
        ProductPhase::ValidatingPrior => "ValidatingPrior",
        ProductPhase::OpeningRestore => "OpeningRestore",
        ProductPhase::CleaningFailedRestore => "CleaningFailedRestore",
        ProductPhase::ValidatingResume => "ValidatingResume",
        ProductPhase::ClosingResume => "ClosingResume",
        ProductPhase::OpeningResume => "OpeningResume",
        ProductPhase::CleaningFailedResume => "CleaningFailedResume",
        ProductPhase::ErrorWithActiveRestored => "ErrorWithActiveRestored",
        ProductPhase::ErrorWithoutActive => "ErrorWithoutActive",
        ProductPhase::Disconnected => "Disconnected",
        ProductPhase::Recovering => "Recovering",
        ProductPhase::SelectionRequired => "SelectionRequired",
        ProductPhase::Stopping => "Stopping",
        ProductPhase::ShutdownReady => "ShutdownReady",
    }
}
/// Length is capped before copying. JSON serialization escapes control characters.
pub(crate) fn bounded_text(value: &str, bytes: usize) -> String {
    bounded_str(value, bytes).to_owned()
}
pub(crate) fn bounded_str(value: &str, bytes: usize) -> &str {
    let mut end = value.len().min(bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}
