//! Value-only errors shared by product state and concrete adapters.

use serde::Serialize;

use super::state::{DraftSettings, FilterAttemptKey};

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
    Filters,
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
    pub filter: Option<Box<FilterFailure>>,
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
            filter: None,
            diagnostic: diagnostic.into(),
        }
    }

    pub fn with_evidence(mut self, evidence: Option<BackendEvidence>) -> Self {
        self.evidence = evidence;
        self
    }

    pub fn with_filter(mut self, failure: FilterFailure) -> Self {
        self.filter = Some(Box::new(failure));
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum FilterConfirmationFailure {
    Deadline,
    MissingReconfig,
    ProgressUnavailable,
    TimeDiscontinuity,
    EvidenceLost,
    BackendUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum FilterErrorKind {
    Prevalidation,
    CatalogUnavailable,
    CommandSubmission { mpv_error: i32 },
    CommandRejected { mpv_error: i32 },
    RuntimeGraph,
    Unconfirmed { reason: FilterConfirmationFailure },
}

/// Frozen user-facing metadata. Ordinals refer to the complete chain, including
/// disabled entries; labels are inert text, never backend syntax.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FilterEntryMetadata {
    pub ordinal: usize,
    pub label: String,
    pub enabled: bool,
}

/// A retained native record; classification always precedes text truncation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FilterDiagnosticRecord {
    pub sequence: u64,
    pub prefix: String,
    pub level: String,
    pub text: String,
    pub truncated: bool,
}

/// Cold snapshot of a single pass, not a second mutable diagnostic ring.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FilterAttemptDiagnostics {
    /// Absent only for prevalidation before a physical owner has been allocated.
    /// Native observation and fault snapshots always retain their exact key.
    pub key: Option<FilterAttemptKey>,
    pub entries: Vec<FilterEntryMetadata>,
    pub records: Vec<FilterDiagnosticRecord>,
    pub native_evidence_lost: bool,
    pub truncated: bool,
    pub dropped_context: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FilterFailure {
    pub kind: FilterErrorKind,
    pub attributed_ordinal: Option<usize>,
    pub requires_fresh_owner: bool,
    pub diagnostics: FilterAttemptDiagnostics,
}

/// Owner diagnostic retention bounds. Native loss and failure latches are
/// independent of retention eviction and must survive a cold snapshot.
pub const FILTER_DIAGNOSTIC_RECORDS: usize = 16;
pub const FILTER_DIAGNOSTIC_TEXT_CHARS: usize = 2048;
