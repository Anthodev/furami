//! Nonblocking, bounded ports. Prepared routes remain opaque to product state.

use crate::domain::{
    capture::{AudioError, AudioSourceIdentity, PlaybackGain},
    failure::ApplyFailure,
    state::{
        AttemptId, AttemptKey, DraftSettings, PauseRequestId, ValidationKey, ValidationRequest,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SubmitFailure {
    #[error("bounded submission capacity unavailable")]
    CapacityUnavailable,
    #[error("adapter disconnected")]
    Disconnected,
}

pub struct ValidationResult<P> {
    pub request: ValidationRequest,
    pub result: Result<P, ApplyFailure>,
}

pub trait DraftValidator {
    type Prepared;
    fn begin_validate(&mut self, request: ValidationRequest) -> Result<(), SubmitFailure>;
    fn poll_validation(&mut self) -> Option<ValidationResult<Self::Prepared>>;
    /// Cancellation never discards the terminal result: consuming it drains work.
    fn cancel_validation(&mut self, key: ValidationKey);
    fn shutdown(&mut self);
    /// True only after worker retirement, not merely a cancellation request.
    fn shutdown_complete(&mut self) -> bool;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartFailure {
    NoResourcesCreated(ApplyFailure),
    ResourcesCreated(ApplyFailure),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StopSubmission {
    Accepted,
    AlreadyStopping,
    NoResourcesCreated,
    Blocked(ApplyFailure),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    Replace,
    Failed,
    Close,
    Quit,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImmediateIntent {
    SetPaused {
        request: PauseRequestId,
        paused: bool,
    },
    SetGain(PlaybackGain),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitStatus {
    Accepted,
    StaleGeneration,
    NotReady,
    Closing,
    CapacityExceeded,
}

pub trait SessionRunner {
    type Prepared;
    fn begin_open(
        &mut self,
        key: AttemptKey,
        prepared: Self::Prepared,
        gain: PlaybackGain,
    ) -> Result<(), StartFailure>;
    fn stop(&mut self, attempt: AttemptId, reason: StopReason) -> StopSubmission;
    /// Terminal acknowledgement/failure must precede coalesced readiness.
    fn poll(&mut self) -> Option<SessionEvent>;
    fn submit_immediate(&mut self, attempt: AttemptId, intent: ImmediateIntent) -> SubmitStatus;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FactStatus {
    Unverified,
    ObservedCompatible,
    Approximate,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationSummary {
    pub captured_fourcc: FactStatus,
    pub decoded_size: FactStatus,
    pub nominal_rate: FactStatus,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioOutcome {
    Disabled,
    Active { source: AudioSourceIdentity },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenReceipt {
    pub settings: DraftSettings,
    pub verification: VerificationSummary,
    pub audio: AudioOutcome,
}
impl OpenReceipt {
    pub fn matches(&self, settings: &DraftSettings) -> bool {
        if &self.settings != settings {
            return false;
        }
        match (&settings.audio, &self.audio) {
            (crate::domain::capture::AudioSelection::Disabled { .. }, AudioOutcome::Disabled) => {
                true
            }
            (
                crate::domain::capture::AudioSelection::Enabled { source: selected },
                AudioOutcome::Active { source },
            ) => selected == source,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioDiagnostic {
    Disabled,
    Opening,
    Active,
    RestartRequired(AudioError),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    OpenVerified {
        key: AttemptKey,
        receipt: OpenReceipt,
    },
    OpenFailed {
        key: AttemptKey,
        failure: ApplyFailure,
    },
    SessionFailed {
        attempt: AttemptId,
        failure: ApplyFailure,
    },
    SessionEnded {
        attempt: AttemptId,
    },
    PauseObserved {
        attempt: AttemptId,
        request: PauseRequestId,
        paused: bool,
    },
    OwnerStopped {
        attempt: AttemptId,
        outcome: Result<(), ApplyFailure>,
    },
    NativeReleased {
        attempt: AttemptId,
    },
    CleanupBlocked {
        attempt: AttemptId,
        failure: ApplyFailure,
    },
    AudioDiagnostic {
        attempt: AttemptId,
        status: AudioDiagnostic,
    },
}
