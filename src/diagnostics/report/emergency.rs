//! Independently bounded fallback, even for oversized injected public DTOs.
use super::*;
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct EmergencyFailures {
    validation: Option<EmergencyFailure>,
    candidate: Option<EmergencyFailure>,
    restore: Option<EmergencyFailure>,
    resume: Option<EmergencyFailure>,
    incumbent: Option<EmergencyFailure>,
    cleanup: Vec<EmergencyFailure>,
}
impl EmergencyFailures {
    pub(super) fn is_empty(&self) -> bool {
        self.validation.is_none()
            && self.candidate.is_none()
            && self.restore.is_none()
            && self.resume.is_none()
            && self.incumbent.is_none()
            && self.cleanup.is_empty()
    }
}
pub(super) fn unavailable<T>(value: &Observation<T>) -> bool {
    matches!(value, Observation::Unavailable { .. })
}
#[derive(Serialize)]
struct EmergencyFailure {
    identity: SessionIdentity,
    phase_at_failure: &'static str,
    correlation: FailureCorrelation,
    requested: CaptureTuple,
    category: crate::domain::failure::FailureCategory,
    stage: crate::domain::failure::Stage,
    cause: crate::domain::failure::Cause,
    evidence: Option<BackendProof>,
}
impl From<&FailureContext> for EmergencyFailure {
    fn from(value: &FailureContext) -> Self {
        Self {
            identity: value.identity,
            phase_at_failure: emergency_phase(value.phase_at_failure),
            correlation: value.correlation.clone(),
            requested: safe_tuple(&value.requested),
            category: value.category,
            stage: value.stage,
            cause: value.cause,
            evidence: value.evidence.clone(),
        }
    }
}
impl From<&FailureLanes> for EmergencyFailures {
    fn from(value: &FailureLanes) -> Self {
        Self {
            validation: value.validation.as_ref().map(EmergencyFailure::from),
            candidate: value.candidate.as_ref().map(EmergencyFailure::from),
            restore: value.restore.as_ref().map(EmergencyFailure::from),
            resume: value.resume.as_ref().map(EmergencyFailure::from),
            incumbent: value.incumbent.as_ref().map(EmergencyFailure::from),
            cleanup: value
                .cleanup
                .iter()
                .take(6)
                .map(EmergencyFailure::from)
                .collect(),
        }
    }
}
fn safe_tuple(value: &CaptureTuple) -> CaptureTuple {
    let mut requested = value.requested;
    requested.format_policy = match requested.format_policy {
        "Auto" => "Auto",
        "Manual" => "Manual",
        _ => "Unknown",
    };
    CaptureTuple {
        vendor_id: value.vendor_id,
        product_id: value.product_id,
        requested,
        resolved_fourcc: value.resolved_fourcc,
        expected_card_model: expected_card_model(value.vendor_id, value.product_id)
            .map(|code| Observation::Observed {
                value: ExpectedModel {
                    code,
                    label: code.label(),
                },
            })
            .unwrap_or(Observation::Unavailable {
                reason: UnavailableReason::UnknownModel,
            }),
    }
}
pub(super) fn capture(value: &Observation<CaptureTuple>) -> Observation<CaptureTuple> {
    match value {
        Observation::Observed { value } => Observation::Observed {
            value: safe_tuple(value),
        },
        Observation::Unavailable { reason } => Observation::Unavailable { reason: *reason },
    }
}
