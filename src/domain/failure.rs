//! Value-only errors shared by product state and concrete adapters.

use serde::Serialize;

use super::state::DraftSettings;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum Stage {
    Prevalidation,
    InputConstruction,
    Open,
    Negotiation,
    StreamStart,
    Verification,
    Unknown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum Cause {
    RequestedModeRefused,
    Busy,
    Permission,
    Generic,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum BackendOperation {
    OpenDevice,
    SetFormat,
    SetFrameRate,
    StartStreaming,
    Unknown,
}
/// Only populate from structured backend operation/errno reports. Never parse
/// numbers or wording out of mpv text logs; standard libmpv supplies no such proof.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackendEvidence {
    pub operation: BackendOperation,
    pub errno: i32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ValidationLayer {
    Data,
    Identity,
    Mode,
    Input,
    Audio,
    Discovery,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum LifecycleFailure {
    OwnerSpawn,
    SurfaceHandoff,
    SurfaceLoss,
    Acknowledgement,
    NativeRelease,
    Cancellation,
    Quiescence,
    Protocol,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum FailureCategory {
    Validation(ValidationLayer),
    Session,
    Lifecycle(LifecycleFailure),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, thiserror::Error)]
#[error("{category:?}/{stage:?}/{cause:?} during {operation}: {diagnostic}")]
pub struct ApplyFailure {
    pub category: FailureCategory,
    pub stage: Stage,
    pub cause: Cause,
    /// Keep the hot port enum/result compact; allocate the complete snapshot only
    /// when constructing a cold failure. Box serialization remains transparent.
    pub requested: Box<DraftSettings>,
    pub operation: String,
    pub evidence: Option<BackendEvidence>,
    pub diagnostic: String,
}
impl ApplyFailure {
    pub fn new(
        category: FailureCategory,
        stage: Stage,
        cause: Cause,
        requested: DraftSettings,
        operation: impl Into<String>,
        diagnostic: impl Into<String>,
    ) -> Self {
        Self {
            category,
            stage,
            cause,
            requested: Box::new(requested),
            operation: operation.into(),
            evidence: None,
            diagnostic: diagnostic.into(),
        }
    }

    pub fn with_evidence(mut self, evidence: Option<BackendEvidence>) -> Self {
        self.evidence = evidence;
        self
    }
}
