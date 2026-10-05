//! Nonblocking, bounded ports. Prepared routes remain opaque to product state.

use crate::domain::{
    capture::{
        AudioAvailability, AudioEpoch, AudioError, AudioRouteReceipt, AudioSilence,
        AudioSourceIdentity, PlaybackGain, RecoveryCandidate, RecoveryObservation,
        RecoveryWatchTarget, SelectionToken, WatchStamp,
    },
    failure::ApplyFailure,
    state::{
        AttemptId, AttemptKey, DraftSettings, InitialPlayback, PauseRequestId, ValidationKey,
        ValidationRequest,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SubmitFailure {
    #[error("bounded submission capacity unavailable")]
    CapacityUnavailable,
    #[error("adapter disconnected")]
    Disconnected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationOutcome<P> {
    Prepared(P),
    SelectionRequired(Vec<RecoveryCandidate>),
    Failed(ApplyFailure),
}
pub struct ValidationResult<P> {
    pub request: ValidationRequest,
    /// Fresh completed worker observation, not the request's old epoch.
    pub stamp: WatchStamp,
    pub result: ValidationOutcome<P>,
}

pub trait DraftValidator {
    type Prepared;
    /// Successful adapter evidence, including a freshly selected complete identity.
    fn prepared_settings(prepared: &Self::Prepared) -> &DraftSettings;
    fn prepared_stamp(prepared: &Self::Prepared) -> WatchStamp;
    fn begin_validate(&mut self, request: ValidationRequest) -> Result<(), SubmitFailure>;
    fn poll_validation(&mut self) -> Option<ValidationResult<Self::Prepared>>;
    /// Cancellation never discards the terminal result: consuming it drains work.
    fn cancel_validation(&mut self, key: ValidationKey);
    fn watch(&mut self, target: RecoveryWatchTarget) -> Result<(), SubmitFailure>;
    fn poll_recovery(&mut self) -> Option<RecoveryObservation>;
    fn clear_watch(&mut self);
    /// Retire one-opening physical authorization after verified commit or actual
    /// failed/cancelled attempt retirement; never replace the saved identity.
    fn retire_selection(&mut self, token: SelectionToken);
    fn shutdown(&mut self);
    /// True only after validation and observation workers have actually joined.
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImmediateIntent {
    SetPaused {
        request: PauseRequestId,
        paused: bool,
    },
    SetGain(PlaybackGain),
    DetachAudio {
        epoch: AudioEpoch,
    },
    AttachAudio {
        epoch: AudioEpoch,
        source: AudioSourceIdentity,
        stamp: WatchStamp,
    },
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
        playback: InitialPlayback,
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenReadiness {
    Live,
    PausedPrepared,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioOutcome {
    Disabled,
    Silent {
        source: AudioSourceIdentity,
        reason: AudioSilence,
    },
    Active {
        source: AudioSourceIdentity,
        route: AudioRouteReceipt,
    },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenReceipt {
    pub settings: DraftSettings,
    pub verification: VerificationSummary,
    pub audio: AudioOutcome,
    pub readiness: OpenReadiness,
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
                AudioOutcome::Active { source, .. } | AudioOutcome::Silent { source, .. },
            ) => selected == source,
            _ => false,
        }
    }
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
    StreamEnded {
        attempt: AttemptId,
        reason: i32,
        error: i32,
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
    AudioAvailability {
        attempt: AttemptId,
        status: AudioAvailability,
    },
    AudioDetached {
        attempt: AttemptId,
        epoch: AudioEpoch,
        outcome: Result<(), AudioError>,
    },
}
