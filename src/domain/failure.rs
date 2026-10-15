//! Value-only errors shared by product state and concrete adapters.

use serde::Serialize;

use super::state::{DraftSettings, TreatmentAttemptKey};

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
    Renderer,
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
#[derive(Clone, Debug, PartialEq, Serialize, thiserror::Error)]
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
    pub treatment: Option<Box<TreatmentFailure>>,
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
            treatment: None,
            diagnostic: diagnostic.into(),
        }
    }

    pub fn with_evidence(mut self, evidence: Option<BackendEvidence>) -> Self {
        self.evidence = evidence;
        self
    }

    pub fn with_treatment(mut self, failure: TreatmentFailure) -> Self {
        self.treatment = Some(Box::new(failure));
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum RendererOperation {
    Snapshot,
    Set,
    Readback,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum RendererErrorKind {
    CatalogUnavailable,
    UnsupportedValue,
    Submission {
        operation: RendererOperation,
        mpv_error: i32,
    },
    Rejected {
        mpv_error: i32,
    },
    PropertyUnavailable {
        operation: RendererOperation,
        mpv_error: i32,
    },
    MalformedReadback,
    ReadbackMismatch,
    IncumbentMismatch,
    Deadline {
        operation: RendererOperation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RendererFailure {
    pub property: super::render::RendererProperty,
    pub kind: RendererErrorKind,
    pub diagnostic: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum TreatmentFailureDisposition {
    Untouched,
    RestoreSameOwner,
    RestoreFreshOwner,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum TreatmentConfirmationFailure {
    Deadline,
    EvidenceLost,
    BackendUnavailable,
    PlaybackChanged,
    ProofMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum TreatmentErrorKind {
    Renderer(RendererFailure),
    Unconfirmed {
        reason: TreatmentConfirmationFailure,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TreatmentFailure {
    pub key: Option<TreatmentAttemptKey>,
    pub kind: TreatmentErrorKind,
    pub disposition: TreatmentFailureDisposition,
}
