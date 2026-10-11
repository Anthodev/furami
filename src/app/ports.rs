//! Nonblocking, bounded ports. Prepared routes remain opaque to product state.

use crate::domain::{
    capture::{
        AudioAvailability, AudioEpoch, AudioError, AudioSilence, AudioSourceIdentity, PlaybackGain,
        RecoveryCandidate, RecoveryObservation, RecoveryWatchTarget, SelectionToken, WatchStamp,
    },
    failure::{ApplyFailure, TreatmentFailure},
    output::OutputPlan,
    state::{
        AttemptId, AttemptKey, DraftSettings, InitialPlayback, PauseRequestId, TreatmentAttemptKey,
        TreatmentConfirmation, TreatmentPass, ValidationKey, ValidationRequest,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SubmitFailure {
    #[error("bounded submission capacity unavailable")]
    CapacityUnavailable,
    #[error("adapter disconnected")]
    Disconnected,
}

#[derive(Clone, Debug, PartialEq)]
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

#[derive(Clone, Debug, PartialEq)]
pub enum StartFailure {
    NoResourcesCreated(ApplyFailure),
    ResourcesCreated(ApplyFailure),
}
#[derive(Clone, Debug, PartialEq)]
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
    SetOutput(OutputPlan),
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
    type PreparedTreatments;
    /// Pure compilation against capabilities observed at startup, without I/O.
    fn prepare_treatments(
        &mut self,
        settings: &DraftSettings,
    ) -> Result<Self::PreparedTreatments, ApplyFailure>;
    fn begin_open(
        &mut self,
        key: AttemptKey,
        prepared: Self::Prepared,
        treatments: Self::PreparedTreatments,
        gain: PlaybackGain,
        playback: InitialPlayback,
        output: OutputPlan,
    ) -> Result<(), StartFailure>;
    fn stop(&mut self, attempt: AttemptId, reason: StopReason) -> StopSubmission;
    /// Terminal acknowledgement/failure must precede coalesced readiness.
    fn poll(&mut self) -> Option<SessionEvent>;
    fn submit_immediate(&mut self, attempt: AttemptId, intent: ImmediateIntent) -> SubmitStatus;
    fn submit_treatments(
        &mut self,
        key: TreatmentAttemptKey,
        treatments: Self::PreparedTreatments,
    ) -> SubmitStatus;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FactStatus {
    Unverified,
    ObservedCompatible,
    Approximate,
    /// Application-configured playback timing, not capture certification.
    Configured,
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
        route: crate::media::loopback::LoopbackReceipt,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub struct OpenReceipt {
    pub settings: DraftSettings,
    pub verification: VerificationSummary,
    pub audio: AudioOutcome,
    pub readiness: OpenReadiness,
    pub treatments: TreatmentConfirmation,
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
                AudioOutcome::Active { route },
            ) => selected == route.source(),
            (
                crate::domain::capture::AudioSelection::Enabled { source: selected },
                AudioOutcome::Silent { source, .. },
            ) => selected == source,
            _ => false,
        }
    }
    pub fn matches_treatments(&self, key: AttemptKey) -> bool {
        self.treatments.key()
            == TreatmentAttemptKey {
                apply: key.apply,
                attempt: key.attempt,
                pass: TreatmentPass::Open,
            }
            && self.treatments.renderer().settings() == &self.settings.renderer
            && matches!(
                (self.readiness, self.treatments.playback()),
                (OpenReadiness::Live, InitialPlayback::Live)
                    | (OpenReadiness::PausedPrepared, InitialPlayback::Paused)
            )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    TreatmentResult {
        key: TreatmentAttemptKey,
        result: Result<TreatmentConfirmation, Box<TreatmentFailure>>,
    },
    TreatmentFault {
        key: TreatmentAttemptKey,
        failure: Box<TreatmentFailure>,
    },
    OpenVerified {
        key: AttemptKey,
        receipt: Box<OpenReceipt>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        capture::{
            AudioSelection, CaptureMode, CapturedFourCc, DeviceIdentity, FrameRate, FrameSize,
            ModeRequest, UsbTopology,
        },
        render::RendererSettings,
        state::{
            ApplyId, AttemptKey, AttemptPurpose, FilterConfirmation, FilterTreatmentProof,
            InitialPlayback, RendererConfirmation, TreatmentAttemptKey, TreatmentConfirmation,
            TreatmentPass,
        },
    };

    fn settings() -> DraftSettings {
        DraftSettings {
            filters: crate::domain::filters::FilterChain::default(),
            video: ModeRequest {
                identity: DeviceIdentity::new(
                    0x32ed,
                    0x3701,
                    UsbTopology::new(
                        "controller".into(),
                        vec![std::num::NonZeroU8::new(1).unwrap()],
                    )
                    .unwrap(),
                    Some("serial".into()),
                )
                .unwrap(),
                mode: CaptureMode {
                    captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
                    size: FrameSize::new(1280, 720).unwrap(),
                    rate: FrameRate::new(60, 1).unwrap(),
                },
            },
            audio: AudioSelection::default(),
            renderer: RendererSettings::default(),
        }
    }

    fn key() -> AttemptKey {
        AttemptKey {
            apply: ApplyId::new(1).unwrap(),
            attempt: AttemptId::new(2).unwrap(),
            purpose: AttemptPurpose::Recovery,
        }
    }

    fn aggregate(open: AttemptKey, playback: InitialPlayback) -> TreatmentConfirmation {
        let treatment = TreatmentAttemptKey {
            apply: open.apply,
            attempt: open.attempt,
            pass: TreatmentPass::Open,
        };
        let (vf_changed, filters) = match playback {
            InitialPlayback::Live => (
                true,
                FilterTreatmentProof::Reconfigured(
                    FilterConfirmation::checked(treatment, 0.0, 2.0, 32).unwrap(),
                ),
            ),
            InitialPlayback::Paused => (
                false,
                FilterTreatmentProof::PreparedPaused { key: treatment },
            ),
        };
        TreatmentConfirmation::checked(
            treatment,
            playback,
            vf_changed,
            filters,
            RendererConfirmation::checked(treatment, settings().renderer.clone()),
        )
        .unwrap()
    }

    fn receipt(open: AttemptKey, playback: InitialPlayback) -> OpenReceipt {
        OpenReceipt {
            settings: settings(),
            verification: VerificationSummary {
                captured_fourcc: FactStatus::Unverified,
                decoded_size: FactStatus::ObservedCompatible,
                nominal_rate: FactStatus::Approximate,
            },
            audio: AudioOutcome::Disabled,
            readiness: match playback {
                InitialPlayback::Live => OpenReadiness::Live,
                InitialPlayback::Paused => OpenReadiness::PausedPrepared,
            },
            treatments: aggregate(open, playback),
        }
    }

    #[test]
    fn matches_treatments_requires_exact_open_key_renderer_and_playback() {
        let open = key();
        assert!(receipt(open, InitialPlayback::Live).matches_treatments(open));
        assert!(receipt(open, InitialPlayback::Paused).matches_treatments(open));

        // A different apply/attempt is never the requested opening.
        let other = AttemptKey {
            attempt: AttemptId::new(open.attempt.get() + 1).unwrap(),
            ..open
        };
        assert!(!receipt(other, InitialPlayback::Live).matches_treatments(open));
        assert!(!receipt(other, InitialPlayback::Paused).matches_treatments(open));
    }

    #[test]
    fn matches_treatments_rejects_renderer_and_readiness_mismatch() {
        let open = key();
        let mut wrong_renderer = receipt(open, InitialPlayback::Live);
        let mut values = settings().renderer.values();
        values.interpolation = !values.interpolation;
        wrong_renderer.settings.renderer = RendererSettings::new(values).unwrap();
        assert!(
            !wrong_renderer.matches_treatments(open),
            "renderer proof must agree with the requested settings"
        );

        let mut wrong_readiness = receipt(open, InitialPlayback::Live);
        wrong_readiness.readiness = OpenReadiness::PausedPrepared;
        assert!(
            !wrong_readiness.matches_treatments(open),
            "readiness must agree with the aggregate playback"
        );

        let mut wrong_paused_readiness = receipt(open, InitialPlayback::Paused);
        wrong_paused_readiness.readiness = OpenReadiness::Live;
        assert!(
            !wrong_paused_readiness.matches_treatments(open),
            "readiness must agree with the aggregate playback"
        );
    }
}
