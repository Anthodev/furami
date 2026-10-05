//! Pure capture-product state. Physical cleanup proofs arrive through app ports.

use std::num::NonZeroU64;

use serde::Serialize;

use super::{
    capture::{
        AudioSelection, LossEvidence, ModeRequest, ObservationEpoch, RecoveryCandidate,
        RecoveryObservation, RecoveryWatchTarget, SelectionToken, VideoPresence, WatchId,
        WatchStamp,
    },
    failure::ApplyFailure,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DraftSettings {
    pub video: ModeRequest,
    pub audio: AudioSelection,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DraftRevision(u64);
impl DraftRevision {
    pub fn get(self) -> u64 {
        self.0
    }
    pub fn new(value: u64) -> Self {
        Self(value)
    }
}
macro_rules! nonzero_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
        pub struct $name(NonZeroU64);
        impl $name {
            pub fn new(value: u64) -> Option<Self> {
                NonZeroU64::new(value).map(Self)
            }
            pub fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}
nonzero_id!(ApplyId);
nonzero_id!(AttemptId);
nonzero_id!(PauseRequestId);
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum InitialPlayback {
    Live,
    Paused,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ReconnectAdmission {
    HealthyNoop,
    Joined(ApplyId),
    Started(ApplyId),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum PlaybackState {
    Live,
    PausePending { request: PauseRequestId },
    Paused,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum AttemptPurpose {
    Candidate,
    Restore,
    Reconnect,
    Resume,
    Recovery,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct AttemptKey {
    pub apply: ApplyId,
    pub attempt: AttemptId,
    pub purpose: AttemptPurpose,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ValidationKey {
    pub apply: ApplyId,
    pub purpose: AttemptPurpose,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ValidationRequest {
    pub key: ValidationKey,
    pub revision: DraftRevision,
    pub settings: DraftSettings,
    pub playback: InitialPlayback,
    pub watch: WatchStamp,
    pub choice: Option<SelectionToken>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Draft {
    pub revision: DraftRevision,
    pub settings: DraftSettings,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AppliedSettings {
    settings: DraftSettings,
}
impl AppliedSettings {
    pub fn settings(&self) -> &DraftSettings {
        &self.settings
    }
}
/// Frozen last-successful state and real loss facts; never derived from the draft.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RecoveryLoss {
    pub applied: AppliedSettings,
    pub playback: InitialPlayback,
    pub evidence: LossEvidence,
    pub failure: ApplyFailure,
    pub stamp: WatchStamp,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Active {
    applied: AppliedSettings,
    key: AttemptKey,
    playback: PlaybackState,
    initial_playback: InitialPlayback,
}
impl Active {
    pub fn applied(&self) -> &AppliedSettings {
        &self.applied
    }
    pub fn attempt(&self) -> AttemptId {
        self.key.attempt
    }
    pub fn playback(&self) -> PlaybackState {
        self.playback
    }
    /// Verified initial readiness, distinct from an ordinary live-origin pause.
    pub fn initial_playback(&self) -> InitialPlayback {
        self.initial_playback
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct FailureReport {
    pub candidate: Option<ApplyFailure>,
    pub restore: Option<ApplyFailure>,
    pub resume: Option<ApplyFailure>,
    pub incumbent: Option<ApplyFailure>,
    pub cleanup: Vec<ApplyFailure>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum CleanupStatus {
    Draining,
    Complete,
    Blocked { failure: ApplyFailure },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ProductPhase {
    Stopped,
    Active,
    PausePending,
    Paused,
    Validating,
    ClosingOld,
    OpeningCandidate,
    CleaningFailedCandidate,
    ValidatingPrior,
    OpeningRestore,
    CleaningFailedRestore,
    ValidatingResume,
    ClosingResume,
    OpeningResume,
    CleaningFailedResume,
    ErrorWithActiveRestored,
    ErrorWithoutActive,
    Disconnected,
    Recovering,
    SelectionRequired,
    Stopping,
    ShutdownReady,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StateIdentity {
    phase: ProductPhase,
    operation: Option<ApplyId>,
    attempt: Option<AttemptId>,
    cleanup: u8,
    playback: Option<PlaybackState>,
    observation: Option<WatchStamp>,
}
impl StateIdentity {
    pub fn phase(self) -> ProductPhase {
        self.phase
    }
    pub fn operation(self) -> Option<ApplyId> {
        self.operation
    }
    pub fn attempt(self) -> Option<AttemptId> {
        self.attempt
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopTarget {
    Session,
    Quit,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopIntent {
    Replace,
    Failed,
    Close,
    Quit,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelEffect {
    Validate(ValidationRequest),
    Open {
        key: AttemptKey,
        request: ValidationRequest,
    },
    Stop {
        attempt: AttemptId,
        reason: StopIntent,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CommandRejection {
    #[error("draft revision is stale")]
    StaleRevision,
    #[error("product state is stale")]
    StaleState,
    #[error("Apply already in progress")]
    ApplyInProgress,
    #[error("cleanup is incomplete")]
    CleanupIncomplete,
    #[error("cleanup is blocked")]
    CleanupBlocked,
    #[error("last-valid reconnect unavailable")]
    ReconnectUnavailable,
    #[error("playback transition already in progress")]
    PlaybackBusy,
    #[error("playback control unavailable")]
    PlaybackUnavailable,
    #[error("shutdown is latched")]
    ShuttingDown,
    #[error("monotonic counter exhausted")]
    CounterExhausted,
    #[error("bounded submission capacity unavailable")]
    CapacityUnavailable,
    #[error("adapter disconnected")]
    Disconnected,
    #[error("recovery selection is stale or unavailable")]
    SelectionUnavailable,
}
#[derive(Clone, Debug, Eq, PartialEq)]
enum Origin {
    Stopped,
    Active,
    Restored(Box<RestoredOrigin>),
    Error(Box<FailureReport>),
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct RestoredOrigin {
    failed_candidate: DraftSettings,
    failures: FailureReport,
}
#[derive(Clone, Debug, Eq, PartialEq)]
enum Step {
    Validating { incumbent: Option<Active> },
    ClosingOld,
    Opening { key: AttemptKey },
    Cleaning { key: AttemptKey },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transition {
    request: ValidationRequest,
    requested_target: DraftSettings,
    prior: Option<AppliedSettings>,
    origin: Origin,
    step: Step,
    failures: FailureReport,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductState {
    Stopped,
    Active(Active),
    Applying(Transition),
    Recovering(Transition),
    ErrorWithActiveRestored {
        active: Active,
        failed_candidate: DraftSettings,
        failures: FailureReport,
    },
    ErrorWithoutActive {
        failures: FailureReport,
    },
    Disconnected,
    SelectionRequired,
    Stopping {
        target: StopTarget,
    },
    ShutdownReady,
}
impl ProductState {
    pub fn phase(&self) -> ProductPhase {
        match self {
            Self::Stopped => ProductPhase::Stopped,
            Self::Active(active) => match active.playback {
                PlaybackState::Live => ProductPhase::Active,
                PlaybackState::PausePending { .. } => ProductPhase::PausePending,
                PlaybackState::Paused => ProductPhase::Paused,
            },
            Self::Applying(transition) | Self::Recovering(transition) => {
                if transition.request.key.purpose == AttemptPurpose::Recovery {
                    return ProductPhase::Recovering;
                }
                match (&transition.step, transition.request.key.purpose) {
                    (Step::Validating { .. }, AttemptPurpose::Restore) => {
                        ProductPhase::ValidatingPrior
                    }
                    (Step::Validating { .. }, AttemptPurpose::Resume) => {
                        ProductPhase::ValidatingResume
                    }
                    (Step::Validating { .. }, _) => ProductPhase::Validating,
                    (Step::ClosingOld, AttemptPurpose::Resume) => ProductPhase::ClosingResume,
                    (Step::ClosingOld, _) => ProductPhase::ClosingOld,
                    (Step::Opening { .. }, AttemptPurpose::Restore) => ProductPhase::OpeningRestore,
                    (Step::Opening { .. }, AttemptPurpose::Resume) => ProductPhase::OpeningResume,
                    (Step::Opening { .. }, _) => ProductPhase::OpeningCandidate,
                    (Step::Cleaning { .. }, AttemptPurpose::Restore) => {
                        ProductPhase::CleaningFailedRestore
                    }
                    (Step::Cleaning { .. }, AttemptPurpose::Resume) => {
                        ProductPhase::CleaningFailedResume
                    }
                    (Step::Cleaning { .. }, _) => ProductPhase::CleaningFailedCandidate,
                }
            }
            Self::ErrorWithActiveRestored { .. } => ProductPhase::ErrorWithActiveRestored,
            Self::ErrorWithoutActive { .. } => ProductPhase::ErrorWithoutActive,
            Self::Disconnected => ProductPhase::Disconnected,
            Self::SelectionRequired => ProductPhase::SelectionRequired,
            Self::Stopping { .. } => ProductPhase::Stopping,
            Self::ShutdownReady => ProductPhase::ShutdownReady,
        }
    }
}

pub struct ProductModel {
    draft: Draft,
    state: ProductState,
    last_valid: Option<AppliedSettings>,
    owned_attempt: Option<AttemptId>,
    cleanup: CleanupStatus,
    next_apply: u64,
    next_attempt: u64,
    next_pause: u64,
    // A single unadmitted reservation is not playback state. Replacements and
    // shutdown discard it so a rejected/late submission cannot mark a pause.
    prepared_pause: Option<(AttemptId, PauseRequestId)>,
    last_operation: Option<ApplyId>,
    quitting: bool,
    validation_rejection: Option<ValidationRejection>,
    completed_failures: Option<FailureReport>,
    watch_target: Option<RecoveryWatchTarget>,
    observation: Option<RecoveryObservation>,
    next_watch: u64,
    consumed_removal: Option<ObservationEpoch>,
    recovery: Option<RecoveryLoss>,
    recovery_candidates: Vec<RecoveryCandidate>,
    loss_attempt: Option<AttemptId>,
    recovery_attempted: Option<WatchStamp>,
    recovery_apply: Option<ApplyId>,
    // Fallback reconnect intent when no frozen RecoveryLoss exists. Pause
    // admission retains Paused; a validated fresh Resume open replaces it.
    last_playback: InitialPlayback,
}
impl ProductModel {
    pub fn new(settings: DraftSettings) -> Self {
        Self {
            draft: Draft {
                revision: DraftRevision::default(),
                settings,
            },
            state: ProductState::Stopped,
            last_valid: None,
            owned_attempt: None,
            cleanup: CleanupStatus::Complete,
            next_apply: 0,
            next_attempt: 0,
            next_pause: 0,
            prepared_pause: None,
            last_operation: None,
            quitting: false,
            validation_rejection: None,
            completed_failures: None,
            watch_target: None,
            observation: None,
            next_watch: 0,
            consumed_removal: None,
            recovery: None,
            recovery_candidates: Vec::new(),
            loss_attempt: None,
            recovery_attempted: None,
            recovery_apply: None,
            last_playback: InitialPlayback::Live,
        }
    }
    pub fn draft(&self) -> &Draft {
        &self.draft
    }
    pub fn state(&self) -> &ProductState {
        &self.state
    }
    pub fn phase(&self) -> ProductPhase {
        self.state.phase()
    }
    pub fn last_valid(&self) -> Option<&AppliedSettings> {
        self.last_valid.as_ref()
    }
    pub fn cleanup(&self) -> &CleanupStatus {
        &self.cleanup
    }
    pub fn validation_rejection(&self) -> Option<&ValidationRejection> {
        self.validation_rejection.as_ref()
    }
    pub fn active(&self) -> Option<&Active> {
        match &self.state {
            ProductState::Active(active) | ProductState::ErrorWithActiveRestored { active, .. } => {
                Some(active)
            }
            ProductState::Applying(Transition {
                step: Step::Validating { incumbent },
                ..
            }) => incumbent.as_ref(),
            _ => None,
        }
    }
    pub fn failures(&self) -> Option<&FailureReport> {
        match &self.state {
            ProductState::Applying(transition) | ProductState::Recovering(transition) => {
                Some(&transition.failures)
            }
            ProductState::ErrorWithActiveRestored { failures, .. }
            | ProductState::ErrorWithoutActive { failures } => Some(failures),
            ProductState::Disconnected
            | ProductState::SelectionRequired
            | ProductState::Active(_)
            | ProductState::Stopped
            | ProductState::Stopping { .. }
            | ProductState::ShutdownReady => self.completed_failures.as_ref(),
        }
    }
    pub fn state_identity(&self) -> StateIdentity {
        StateIdentity {
            phase: self.phase(),
            operation: self.last_operation,
            attempt: self.owned_attempt,
            cleanup: match self.cleanup {
                CleanupStatus::Complete => 0,
                CleanupStatus::Draining => 1,
                CleanupStatus::Blocked { .. } => 2,
            },
            playback: self.active().map(Active::playback),
            observation: self
                .observation
                .as_ref()
                .map(|observation| observation.stamp),
        }
    }
    pub fn can_apply(&self) -> bool {
        !self.quitting
            && matches!(self.cleanup, CleanupStatus::Complete)
            && !self
                .active()
                .is_some_and(|active| matches!(active.playback, PlaybackState::PausePending { .. }))
            && matches!(
                self.state,
                ProductState::Stopped
                    | ProductState::Active(_)
                    | ProductState::ErrorWithActiveRestored { .. }
                    | ProductState::ErrorWithoutActive { .. }
                    | ProductState::Disconnected
                    | ProductState::SelectionRequired
            )
    }
    pub fn can_reconnect(&self) -> bool {
        !self.quitting
            && !matches!(
                self.state,
                ProductState::Stopping { .. } | ProductState::ShutdownReady
            )
            && !matches!(self.cleanup, CleanupStatus::Blocked { .. })
            && self.last_valid.is_some()
            && (matches!(self.cleanup, CleanupStatus::Complete) || self.recovery.is_some())
    }
    pub fn can_restart(&self) -> bool {
        self.can_apply() && self.last_valid.is_some()
    }
    pub fn shutdown_ready(&self) -> bool {
        matches!(self.state, ProductState::ShutdownReady)
    }
    pub fn validation_request(&self) -> Option<&ValidationRequest> {
        match &self.state {
            ProductState::Applying(transition) | ProductState::Recovering(transition)
                if matches!(transition.step, Step::Validating { .. }) =>
            {
                Some(&transition.request)
            }
            _ => None,
        }
    }
    pub fn opening(&self) -> Option<(AttemptKey, &DraftSettings)> {
        match &self.state {
            ProductState::Applying(transition) | ProductState::Recovering(transition) => {
                match transition.step {
                    Step::Opening { key } => Some((key, &transition.request.settings)),
                    _ => None,
                }
            }
            _ => None,
        }
    }
    pub fn recovery(&self) -> Option<&RecoveryLoss> {
        self.recovery.as_ref()
    }
    pub fn recovery_candidates(&self) -> &[RecoveryCandidate] {
        &self.recovery_candidates
    }
    pub fn watch_target(&self) -> Option<&RecoveryWatchTarget> {
        self.watch_target.as_ref()
    }
    pub fn observation(&self) -> Option<&RecoveryObservation> {
        self.observation.as_ref()
    }
    pub fn opening_request(&self) -> Option<&ValidationRequest> {
        match &self.state {
            ProductState::Applying(transition) | ProductState::Recovering(transition)
                if matches!(transition.step, Step::Opening { .. }) =>
            {
                Some(&transition.request)
            }
            _ => None,
        }
    }
    fn ensure_watch(&mut self, settings: &DraftSettings) -> Result<WatchStamp, CommandRejection> {
        let replace = self.watch_target.as_ref().is_none_or(|target| {
            self.owned_attempt.is_none()
                && (target.video != settings.video || target.audio != settings.audio)
        });
        if replace {
            let next = self
                .next_watch
                .checked_add(1)
                .ok_or(CommandRejection::CounterExhausted)?;
            let watch = WatchId::new(next).ok_or(CommandRejection::CounterExhausted)?;
            self.next_watch = next;
            self.watch_target = Some(RecoveryWatchTarget {
                watch,
                video: settings.video.clone(),
                audio: settings.audio.clone(),
            });
            self.observation = None;
            self.consumed_removal = None;
        }
        let watch = self
            .watch_target
            .as_ref()
            .ok_or(CommandRejection::Disconnected)?
            .watch;
        // This first epoch identifies the requested scan, not subscription readiness.
        // The coordinator cannot submit it until the authoritative observation arrives.
        Ok(self.observation.as_ref().map_or(
            WatchStamp {
                watch,
                epoch: ObservationEpoch::new(1).ok_or(CommandRejection::CounterExhausted)?,
            },
            |observation| observation.stamp,
        ))
    }
    /// A selected complete identity becomes the logical target only after verified open.
    pub fn committed_watch(&mut self) -> Result<Option<RecoveryWatchTarget>, CommandRejection> {
        let Some(settings) = self.last_valid.as_ref().map(|applied| &applied.settings) else {
            return Ok(None);
        };
        if self
            .watch_target
            .as_ref()
            .is_some_and(|target| target.video == settings.video && target.audio == settings.audio)
        {
            return Ok(None);
        }
        let settings = settings.clone();
        let next = self
            .next_watch
            .checked_add(1)
            .ok_or(CommandRejection::CounterExhausted)?;
        let watch = WatchId::new(next).ok_or(CommandRejection::CounterExhausted)?;
        self.next_watch = next;
        let target = RecoveryWatchTarget {
            watch,
            video: settings.video,
            audio: settings.audio,
        };
        self.watch_target = Some(target.clone());
        self.observation = None;
        self.consumed_removal = None;
        Ok(Some(target))
    }
    /// Refresh only a still-current validation from real completed watcher evidence.
    pub fn refresh_validation_stamp(&mut self, stamp: WatchStamp) -> Option<ValidationRequest> {
        if self.watch_target.as_ref()?.watch != stamp.watch {
            return None;
        }
        let request = match &mut self.state {
            ProductState::Applying(transition) | ProductState::Recovering(transition)
                if matches!(transition.step, Step::Validating { .. }) =>
            {
                &mut transition.request
            }
            _ => return None,
        };
        request.watch = stamp;
        Some(request.clone())
    }
    /// Only successful opaque adapter evidence may replace the complete identity.
    pub fn accept_prepared(
        &mut self,
        request: &ValidationRequest,
        settings: DraftSettings,
        stamp: WatchStamp,
    ) -> Option<ValidationRequest> {
        if self.validation_request() != Some(request)
            || stamp.watch != request.watch.watch
            || stamp.epoch.get() < request.watch.epoch.get()
            || settings.video.mode != request.settings.video.mode
            || settings.audio != request.settings.audio
        {
            return None;
        }
        let transition = match &mut self.state {
            ProductState::Applying(transition) | ProductState::Recovering(transition) => transition,
            _ => return None,
        };
        transition.request.settings = settings;
        transition.request.watch = stamp;
        if matches!(
            transition.request.key.purpose,
            AttemptPurpose::Recovery | AttemptPurpose::Reconnect
        ) {
            self.recovery_attempted = Some(stamp);
        }
        Some(transition.request.clone())
    }
    /// A different explicit candidate must acquire its watch after incumbent retirement.
    /// The already-running subscription then produces an authoritative cutover scan.
    pub fn cutover_validation(&mut self, key: AttemptKey) -> Option<ModelEffect> {
        if self.opening().map(|(current, _)| current) != Some(key)
            || self.owned_attempt != Some(key.attempt)
            || matches!(
                key.purpose,
                AttemptPurpose::Recovery | AttemptPurpose::Reconnect
            )
        {
            return None;
        }
        let settings = match &self.state {
            ProductState::Applying(transition) | ProductState::Recovering(transition) => {
                &transition.requested_target
            }
            _ => return None,
        };
        let target = self.watch_target.as_ref()?;
        if target.video == settings.video && target.audio == settings.audio {
            return None;
        }
        let settings = settings.clone();
        let (mut transition, recovering) = self.take_transition()?;
        // No physical resources for this reserved attempt have been created.
        self.owned_attempt = None;
        match self.ensure_watch(&settings) {
            Ok(stamp) => transition.request.watch = stamp,
            Err(_) => {
                let failure = ApplyFailure::new(
                    crate::domain::failure::FailureCategory::Lifecycle(
                        crate::domain::failure::LifecycleFailure::Protocol,
                    ),
                    crate::domain::failure::Stage::Unknown,
                    crate::domain::failure::Cause::Generic,
                    settings,
                    "watch_cutover",
                    "watch counter exhausted",
                );
                transition.failures.candidate = Some(failure);
                self.state = ProductState::ErrorWithoutActive {
                    failures: transition.failures,
                };
                return None;
            }
        }
        transition.request.settings = settings;
        transition.step = Step::Validating { incumbent: None };
        let effect = ModelEffect::Validate(transition.request.clone());
        self.put_transition(transition, recovering);
        Some(effect)
    }
    pub fn video_lost(
        &mut self,
        attempt: AttemptId,
        evidence: LossEvidence,
        failure: ApplyFailure,
    ) -> Option<ModelEffect> {
        if self.quitting
            || matches!(
                self.state,
                ProductState::Stopping { .. } | ProductState::ShutdownReady
            )
        {
            return None;
        }
        if self.loss_attempt == Some(attempt) {
            if let LossEvidence::Removed { stamp } = evidence
                && let Some(loss) = &mut self.recovery
                && loss.stamp.watch == stamp.watch
            {
                loss.evidence = LossEvidence::Removed { stamp };
                loss.stamp = stamp;
            }
            return None;
        }
        if self.owned_attempt != Some(attempt) {
            return None;
        }
        if let Some(active) = self.active().cloned() {
            let stamp = self
                .observation
                .as_ref()
                .map(|observation| observation.stamp)
                .or_else(|| {
                    self.watch_target.as_ref().and_then(|target| {
                        ObservationEpoch::new(1).map(|epoch| WatchStamp {
                            watch: target.watch,
                            epoch,
                        })
                    })
                })?;
            let playback = match active.playback {
                PlaybackState::Live => InitialPlayback::Live,
                PlaybackState::Paused | PlaybackState::PausePending { .. } => {
                    InitialPlayback::Paused
                }
            };
            self.last_playback = playback;
            self.recovery = Some(RecoveryLoss {
                applied: active.applied,
                playback,
                evidence,
                failure: failure.clone(),
                stamp,
            });
            self.loss_attempt = Some(attempt);
            self.recovery_attempted = None;
            if let Some(value) = self.next_apply.checked_add(1)
                && let Some(apply) = ApplyId::new(value)
            {
                self.next_apply = value;
                self.recovery_apply = Some(apply);
                if !matches!(self.state, ProductState::Applying(_)) {
                    self.last_operation = Some(apply);
                }
            }
            self.recovery_candidates.clear();
            self.prepared_pause = None;
            self.cleanup = CleanupStatus::Draining;
            if let Some((mut transition, restoring)) = self.take_transition() {
                if let Step::Validating { incumbent } = &mut transition.step {
                    *incumbent = None;
                }
                transition.failures.incumbent = Some(failure);
                self.put_transition(transition, restoring);
            } else {
                let mut report = self.failures().cloned().unwrap_or_default();
                report.incumbent = Some(failure);
                self.completed_failures = Some(report);
                self.state = ProductState::Disconnected;
            }
            return Some(ModelEffect::Stop {
                attempt,
                reason: StopIntent::Failed,
            });
        }
        if let Some((key, _)) = self.opening() {
            // EOF during an unverified open is a real open failure, not a new
            // successfully applied value and not an automatic same-epoch retry.
            return self.open_failed(key, failure);
        }
        None
    }
    pub fn recovery_observed(&mut self, observation: &RecoveryObservation) -> Option<ModelEffect> {
        if self.quitting
            || matches!(
                self.state,
                ProductState::Stopping { .. } | ProductState::ShutdownReady
            )
            || self.watch_target.as_ref().map(|target| target.watch)
                != Some(observation.stamp.watch)
            || self
                .observation
                .as_ref()
                .is_some_and(|old| observation.stamp.epoch.get() < old.stamp.epoch.get())
        {
            return None;
        }
        if self
            .observation
            .as_ref()
            .is_some_and(|old| old.stamp.epoch == observation.stamp.epoch)
        {
            return None;
        }
        let removal = observation.last_video_removal.filter(|epoch| {
            epoch.get() <= observation.stamp.epoch.get()
                && self
                    .consumed_removal
                    .is_none_or(|old| epoch.get() > old.get())
        });
        self.observation = Some(observation.clone());
        if let Some(loss) = &mut self.recovery
            && self.watch_target.as_ref().is_some_and(|target| {
                target.video == loss.applied.settings.video
                    && target.audio == loss.applied.settings.audio
            })
        {
            loss.stamp = observation.stamp;
        }
        if let Some(epoch) = removal {
            self.consumed_removal = Some(epoch);
            let stamp = WatchStamp {
                watch: observation.stamp.watch,
                epoch,
            };
            if let Some(loss) = &mut self.recovery
                && self.watch_target.as_ref().is_some_and(|target| {
                    target.video == loss.applied.settings.video
                        && target.audio == loss.applied.settings.audio
                })
            {
                loss.evidence = LossEvidence::Removed { stamp };
                loss.stamp = observation.stamp;
            }
            if let Some(attempt) = self.active().map(Active::attempt) {
                let settings = self.active()?.applied.settings.clone();
                let failure = ApplyFailure::new(
                    crate::domain::failure::FailureCategory::Session,
                    crate::domain::failure::Stage::Unknown,
                    crate::domain::failure::Cause::Generic,
                    settings,
                    "video_removed",
                    "matching capture device removal observed",
                );
                return self.video_lost(attempt, LossEvidence::Removed { stamp }, failure);
            }
            // Removal high-water invalidates an opening/validation even when
            // the coalesced latest inventory is already Present again.
            if let Some((mut transition, restoring)) = self.take_transition() {
                if transition.request.watch.watch == stamp.watch
                    && epoch.get() >= transition.request.watch.epoch.get()
                {
                    self.recovery_attempted = None;
                    if let Step::Opening { key } = transition.step {
                        self.cleanup = CleanupStatus::Draining;
                        self.completed_failures = Some(transition.failures);
                        self.state = if self.recovery.is_some() {
                            ProductState::Disconnected
                        } else {
                            ProductState::ErrorWithoutActive {
                                failures: self.completed_failures.clone().unwrap_or_default(),
                            }
                        };
                        return Some(ModelEffect::Stop {
                            attempt: key.attempt,
                            reason: StopIntent::Failed,
                        });
                    }
                    if self.recovery.is_some() {
                        self.completed_failures = Some(transition.failures);
                        self.state = ProductState::Disconnected;
                    } else {
                        let failure = ApplyFailure::new(
                            crate::domain::failure::FailureCategory::Validation(
                                crate::domain::failure::ValidationLayer::Discovery,
                            ),
                            crate::domain::failure::Stage::Prevalidation,
                            crate::domain::failure::Cause::Generic,
                            transition.request.settings.clone(),
                            "validation_removed",
                            "matching removal invalidated validation",
                        );
                        transition.failures.candidate = Some(failure);
                        self.state = ProductState::ErrorWithoutActive {
                            failures: transition.failures,
                        };
                    }
                } else {
                    self.put_transition(transition, restoring);
                }
            }
        }
        let admitted_choice = self
            .validation_request()
            .or_else(|| self.opening_request())
            .is_some_and(|request| request.choice.is_some());
        // A fresh scan may remain ambiguous because automatic resolution is
        // deliberately strict. It does not revoke an already admitted physical
        // authorization; the worker/owner still fresh-checks that exact route.
        // Positive removal, Absent, and Unknown have already won or still win.
        if admitted_choice && matches!(observation.video, VideoPresence::Ambiguous(_)) {
            return None;
        }
        if !matches!(observation.video, VideoPresence::Present) {
            if let Some((key, settings)) = self.opening() {
                let failure = match &observation.video {
                    VideoPresence::Unknown(failure) => failure.clone(),
                    _ => ApplyFailure::new(
                        crate::domain::failure::FailureCategory::Validation(
                            crate::domain::failure::ValidationLayer::Discovery,
                        ),
                        crate::domain::failure::Stage::Prevalidation,
                        crate::domain::failure::Cause::Generic,
                        settings.clone(),
                        "opening_presence",
                        "fresh capture inventory no longer admits this open",
                    ),
                };
                return self.open_failed(key, failure);
            }
            if let Some(request) = self.validation_request().cloned()
                && matches!(
                    request.key.purpose,
                    AttemptPurpose::Recovery | AttemptPurpose::Reconnect
                )
            {
                match &observation.video {
                    VideoPresence::Unknown(failure) => {
                        self.validation_failed(request, failure.clone())
                    }
                    VideoPresence::Ambiguous(candidates) => {
                        self.selection_required(&request, candidates.clone())
                    }
                    VideoPresence::Absent => {
                        self.completed_failures = self.failures().cloned();
                        self.state = ProductState::Disconnected;
                    }
                    VideoPresence::Present => {}
                }
            }
        }
        self.continue_recovery()
    }
    /// Called after accepted validation results and actual physical barriers drain.
    pub fn continue_recovery(&mut self) -> Option<ModelEffect> {
        if self.quitting
            || self.owned_attempt.is_some()
            || !matches!(self.cleanup, CleanupStatus::Complete)
            || !matches!(
                self.state,
                ProductState::Disconnected
                    | ProductState::SelectionRequired
                    | ProductState::ErrorWithoutActive { .. }
            )
        {
            return None;
        }
        let loss = self.recovery.as_ref()?;
        let observation = self.observation.as_ref()?;
        if self.recovery_attempted == Some(observation.stamp) {
            return None;
        }
        match &observation.video {
            VideoPresence::Absent => {
                self.recovery_candidates.clear();
                self.state = ProductState::Disconnected;
                None
            }
            VideoPresence::Ambiguous(candidates) => {
                self.recovery_candidates = candidates.clone();
                self.state = ProductState::SelectionRequired;
                None
            }
            VideoPresence::Unknown(failure) => {
                let mut report = self.completed_failures.clone().unwrap_or_default();
                report.candidate = Some(failure.clone());
                self.state = ProductState::ErrorWithoutActive { failures: report };
                self.recovery_attempted = Some(observation.stamp);
                None
            }
            VideoPresence::Present => {
                let settings = loss.applied.settings.clone();
                let stamp = observation.stamp;
                match self.begin(settings, AttemptPurpose::Recovery) {
                    Ok((_, effect)) => {
                        self.recovery_attempted = Some(stamp);
                        Some(effect)
                    }
                    Err(_) => None,
                }
            }
        }
    }
    pub fn choose_recovery(
        &mut self,
        expected: StateIdentity,
        token: SelectionToken,
    ) -> Result<Option<ModelEffect>, CommandRejection> {
        self.check_command(expected)?;
        if !matches!(self.state, ProductState::SelectionRequired)
            || self
                .observation
                .as_ref()
                .map(|observation| observation.stamp)
                != Some(token.stamp)
            || !self
                .recovery_candidates
                .iter()
                .any(|candidate| candidate.token == token)
        {
            return Err(CommandRejection::SelectionUnavailable);
        }
        let settings = self
            .recovery
            .as_ref()
            .ok_or(CommandRejection::SelectionUnavailable)?
            .applied
            .settings
            .clone();
        let (_, _) = self.begin(settings, AttemptPurpose::Recovery)?;
        let transition = match &mut self.state {
            ProductState::Recovering(transition) => transition,
            _ => return Err(CommandRejection::SelectionUnavailable),
        };
        transition.request.choice = Some(token);
        self.recovery_attempted = Some(token.stamp);
        self.recovery_candidates.clear();
        Ok(Some(ModelEffect::Validate(transition.request.clone())))
    }
    pub fn selection_required(
        &mut self,
        request: &ValidationRequest,
        candidates: Vec<RecoveryCandidate>,
    ) {
        if self.validation_request() != Some(request) {
            return;
        }
        let report = self.failures().cloned().unwrap_or_default();
        if let Some(stamp) = candidates.first().map(|candidate| candidate.token.stamp)
            && let Some(observation) = &mut self.observation
            && observation.stamp.watch == stamp.watch
            && observation.stamp.epoch.get() <= stamp.epoch.get()
        {
            observation.stamp = stamp;
            observation.video = VideoPresence::Ambiguous(candidates.clone());
        }
        self.completed_failures = Some(report);
        self.recovery_candidates = candidates;
        self.state = ProductState::SelectionRequired;
    }
    pub fn edit_draft(
        &mut self,
        expected: DraftRevision,
        settings: DraftSettings,
    ) -> Result<DraftRevision, CommandRejection> {
        if self.quitting {
            return Err(CommandRejection::ShuttingDown);
        }
        if expected != self.draft.revision {
            return Err(CommandRejection::StaleRevision);
        }
        let revision = self
            .draft
            .revision
            .0
            .checked_add(1)
            .ok_or(CommandRejection::CounterExhausted)?;
        self.draft = Draft {
            revision: DraftRevision(revision),
            settings,
        };
        Ok(self.draft.revision)
    }
    fn check_command(&self, expected: StateIdentity) -> Result<(), CommandRejection> {
        if self.quitting {
            return Err(CommandRejection::ShuttingDown);
        }
        if expected != self.state_identity() {
            return Err(CommandRejection::StaleState);
        }
        if matches!(
            self.state,
            ProductState::Applying(_) | ProductState::Recovering(_)
        ) {
            return Err(CommandRejection::ApplyInProgress);
        }
        if self
            .active()
            .is_some_and(|active| matches!(active.playback, PlaybackState::PausePending { .. }))
        {
            return Err(CommandRejection::PlaybackBusy);
        }
        match self.cleanup {
            CleanupStatus::Draining => Err(CommandRejection::CleanupIncomplete),
            CleanupStatus::Blocked { .. } => Err(CommandRejection::CleanupBlocked),
            CleanupStatus::Complete if matches!(self.state, ProductState::Stopping { .. }) => {
                Err(CommandRejection::CleanupIncomplete)
            }
            CleanupStatus::Complete => Ok(()),
        }
    }
    pub fn apply(
        &mut self,
        expected: StateIdentity,
        revision: DraftRevision,
    ) -> Result<(ApplyId, ModelEffect), CommandRejection> {
        self.check_command(expected)?;
        if revision != self.draft.revision {
            return Err(CommandRejection::StaleRevision);
        }
        self.begin(self.draft.settings.clone(), AttemptPurpose::Candidate)
    }
    pub fn reconnect(
        &mut self,
        expected: StateIdentity,
    ) -> Result<(ReconnectAdmission, Option<ModelEffect>), CommandRejection> {
        if self.quitting {
            return Err(CommandRejection::ShuttingDown);
        }
        if expected != self.state_identity() {
            return Err(CommandRejection::StaleState);
        }
        if let ProductState::Applying(transition) | ProductState::Recovering(transition) =
            &self.state
            && matches!(
                transition.request.key.purpose,
                AttemptPurpose::Recovery | AttemptPurpose::Reconnect
            )
        {
            return Ok((
                ReconnectAdmission::Joined(transition.request.key.apply),
                None,
            ));
        }
        if self.recovery.is_some() && self.owned_attempt.is_some() {
            let apply = self
                .last_operation
                .ok_or(CommandRejection::ReconnectUnavailable)?;
            return Ok((ReconnectAdmission::Joined(apply), None));
        }
        if self.active().is_some() {
            return Ok((ReconnectAdmission::HealthyNoop, None));
        }
        self.check_command(expected)?;
        let target = self
            .last_valid
            .as_ref()
            .ok_or(CommandRejection::ReconnectUnavailable)?
            .settings
            .clone();
        self.recovery_attempted = None;
        self.recovery_apply = None;
        let (apply, effect) = self.begin(target, AttemptPurpose::Reconnect)?;
        Ok((ReconnectAdmission::Started(apply), Some(effect)))
    }
    pub fn restart(
        &mut self,
        expected: StateIdentity,
    ) -> Result<(ApplyId, ModelEffect), CommandRejection> {
        self.check_command(expected)?;
        if self.active().is_none() {
            self.check_command(expected)?;
            let target = self
                .last_valid
                .as_ref()
                .ok_or(CommandRejection::ReconnectUnavailable)?
                .settings
                .clone();
            return self.begin(target, AttemptPurpose::Reconnect);
        }
        let settings = self
            .active()
            .ok_or(CommandRejection::ReconnectUnavailable)?
            .applied
            .settings
            .clone();
        self.begin(settings, AttemptPurpose::Candidate)
    }
    fn playback_attempt(&self, attempt: AttemptId) -> Result<&Active, CommandRejection> {
        let active = match &self.state {
            ProductState::Active(active) | ProductState::ErrorWithActiveRestored { active, .. } => {
                active
            }
            _ => return Err(CommandRejection::PlaybackUnavailable),
        };
        if active.attempt() != attempt {
            return Err(CommandRejection::StaleState);
        }
        Ok(active)
    }
    /// Reserve correlation before media admission without publishing a pause.
    pub fn prepare_pause(
        &mut self,
        attempt: AttemptId,
    ) -> Result<PauseRequestId, CommandRejection> {
        self.check_command(self.state_identity())?;
        if self.playback_attempt(attempt)?.playback != PlaybackState::Live {
            return Err(CommandRejection::PlaybackUnavailable);
        }
        let value = self
            .next_pause
            .checked_add(1)
            .ok_or(CommandRejection::CounterExhausted)?;
        let request = PauseRequestId::new(value).ok_or(CommandRejection::CounterExhausted)?;
        self.next_pause = value;
        self.prepared_pause = Some((attempt, request));
        Ok(request)
    }
    /// Called only after the matching owner command has been admitted.
    pub fn pause_admitted(&mut self, attempt: AttemptId, request: PauseRequestId) -> bool {
        if self.prepared_pause != Some((attempt, request))
            || self.check_command(self.state_identity()).is_err()
            || !self
                .playback_attempt(attempt)
                .is_ok_and(|active| active.playback == PlaybackState::Live)
        {
            return false;
        }
        match &mut self.state {
            ProductState::Active(active) | ProductState::ErrorWithActiveRestored { active, .. } => {
                active.playback = PlaybackState::PausePending { request };
            }
            _ => return false,
        }
        self.prepared_pause = None;
        self.last_playback = InitialPlayback::Paused;
        true
    }
    /// Only a true correlated readback can confirm the admitted pause.
    /// Invalid/false backend readbacks must separately follow session_failed.
    pub fn pause_observed(
        &mut self,
        attempt: AttemptId,
        request: PauseRequestId,
        paused: bool,
    ) -> bool {
        if !paused || self.quitting || !matches!(self.cleanup, CleanupStatus::Complete) {
            return false;
        }
        let active = match &mut self.state {
            ProductState::Active(active) | ProductState::ErrorWithActiveRestored { active, .. } => {
                active
            }
            _ => return false,
        };
        if active.attempt() != attempt
            || active.playback != (PlaybackState::PausePending { request })
        {
            return false;
        }
        active.playback = PlaybackState::Paused;
        true
    }
    /// Resume replaces one paused applied input; it never unpauses the old owner.
    pub fn resume(
        &mut self,
        expected: StateIdentity,
        attempt: AttemptId,
    ) -> Result<(ApplyId, ModelEffect), CommandRejection> {
        self.check_command(expected)?;
        let active = self.playback_attempt(attempt)?;
        if active.playback != PlaybackState::Paused {
            return Err(CommandRejection::PlaybackUnavailable);
        }
        let settings = active.applied.settings.clone();
        self.begin(settings, AttemptPurpose::Resume)
    }
    fn begin(
        &mut self,
        settings: DraftSettings,
        purpose: AttemptPurpose,
    ) -> Result<(ApplyId, ModelEffect), CommandRejection> {
        let value = if purpose == AttemptPurpose::Recovery {
            self.recovery_apply
                .map(ApplyId::get)
                .or_else(|| self.next_apply.checked_add(1))
                .ok_or(CommandRejection::CounterExhausted)?
        } else {
            self.next_apply
                .checked_add(1)
                .ok_or(CommandRejection::CounterExhausted)?
        };
        // Reserve capacity for the only possible rollback before touching resources.
        let required = if purpose == AttemptPurpose::Candidate && self.last_valid.is_some() {
            // Candidate and rollback may each reserve a no-resource attempt
            // before the fresh target-cutover validation.
            4
        } else if purpose == AttemptPurpose::Resume
            && self.watch_target.as_ref().is_some_and(|target| {
                target.video == settings.video && target.audio == settings.audio
            })
        {
            // The immutable Resume target already has the incumbent watch,
            // so no no-resource target cutover can consume another attempt.
            1
        } else {
            2
        };
        self.next_attempt
            .checked_add(required)
            .ok_or(CommandRejection::CounterExhausted)?;
        let apply = if purpose == AttemptPurpose::Recovery {
            self.recovery_apply
                .or_else(|| ApplyId::new(value))
                .ok_or(CommandRejection::CounterExhausted)?
        } else {
            ApplyId::new(value).ok_or(CommandRejection::CounterExhausted)?
        };
        let incumbent = self.active().cloned();
        let watch = self.ensure_watch(&settings)?;
        let playback = if matches!(
            purpose,
            AttemptPurpose::Recovery | AttemptPurpose::Reconnect
        ) {
            self.recovery
                .as_ref()
                .map_or(self.last_playback, |loss| loss.playback)
        } else {
            InitialPlayback::Live
        };
        let origin = match &self.state {
            ProductState::Active(_) => Origin::Active,
            ProductState::ErrorWithActiveRestored {
                failed_candidate,
                failures,
                ..
            } => Origin::Restored(Box::new(RestoredOrigin {
                failed_candidate: failed_candidate.clone(),
                failures: failures.clone(),
            })),
            ProductState::ErrorWithoutActive { failures } => {
                Origin::Error(Box::new(failures.clone()))
            }
            _ => Origin::Stopped,
        };
        let request = ValidationRequest {
            key: ValidationKey { apply, purpose },
            revision: self.draft.revision,
            settings,
            playback,
            watch,
            choice: None,
        };
        if purpose != AttemptPurpose::Recovery || self.recovery_apply.is_none() {
            self.next_apply = value;
        }
        self.last_operation = Some(apply);
        self.validation_rejection = None;
        self.prepared_pause = None;
        let transition = Transition {
            request: request.clone(),
            requested_target: request.settings.clone(),
            prior: self.last_valid.clone(),
            origin,
            step: Step::Validating { incumbent },
            failures: self
                .recovery
                .as_ref()
                .map_or_else(FailureReport::default, |loss| FailureReport {
                    incumbent: Some(loss.failure.clone()),
                    ..FailureReport::default()
                }),
        };
        self.put_transition(transition, purpose == AttemptPurpose::Recovery);
        Ok((apply, ModelEffect::Validate(request)))
    }
    fn take_transition(&mut self) -> Option<(Transition, bool)> {
        if !matches!(
            self.state,
            ProductState::Applying(_) | ProductState::Recovering(_)
        ) {
            return None;
        }
        match std::mem::replace(&mut self.state, ProductState::Stopped) {
            ProductState::Applying(transition) => Some((transition, false)),
            ProductState::Recovering(transition) => Some((transition, true)),
            _ => None,
        }
    }
    fn put_transition(&mut self, transition: Transition, recovering: bool) {
        self.state = if recovering {
            ProductState::Recovering(transition)
        } else {
            ProductState::Applying(transition)
        };
    }
    pub fn validation_succeeded(&mut self, request: &ValidationRequest) -> Option<ModelEffect> {
        if self.validation_request() != Some(request) {
            return None;
        }
        let (mut transition, recovering) = self.take_transition()?;
        if matches!(self.cleanup, CleanupStatus::Blocked { .. }) {
            self.state = ProductState::ErrorWithoutActive {
                failures: transition.failures,
            };
            return None;
        }
        let incumbent = match &mut transition.step {
            Step::Validating { incumbent } => incumbent.take(),
            _ => None,
        };
        if let Some(attempt) = self.owned_attempt {
            transition.step = Step::ClosingOld;
            self.cleanup = CleanupStatus::Draining;
            self.put_transition(transition, recovering);
            return incumbent.map(|_| ModelEffect::Stop {
                attempt,
                reason: StopIntent::Replace,
            });
        }
        self.open_transition(transition, recovering)
    }
    pub fn validation_failed(&mut self, request: ValidationRequest, failure: ApplyFailure) {
        if self.validation_request() != Some(&request) {
            return;
        }
        let Some((mut transition, _)) = self.take_transition() else {
            return;
        };
        self.validation_rejection = Some(ValidationRejection {
            request,
            failure: failure.clone(),
        });
        if transition.request.key.purpose == AttemptPurpose::Restore {
            transition.failures.restore = Some(failure);
            self.state = ProductState::ErrorWithoutActive {
                failures: transition.failures,
            };
            return;
        }
        if matches!(
            transition.request.key.purpose,
            AttemptPurpose::Recovery | AttemptPurpose::Reconnect
        ) {
            transition.failures.candidate = Some(failure);
            self.recovery_attempted = Some(self.observation.as_ref().map_or(
                transition.request.watch,
                |observation| {
                    if observation.stamp.watch == transition.request.watch.watch
                        && observation.stamp.epoch.get() > transition.request.watch.epoch.get()
                    {
                        observation.stamp
                    } else {
                        transition.request.watch
                    }
                },
            ));
            self.state = ProductState::ErrorWithoutActive {
                failures: transition.failures,
            };
            return;
        }
        if self.recovery.is_some() {
            transition.failures.candidate = Some(failure);
            self.recovery_attempted = Some(self.observation.as_ref().map_or(
                transition.request.watch,
                |observation| {
                    if observation.stamp.watch == transition.request.watch.watch
                        && observation.stamp.epoch.get() > transition.request.watch.epoch.get()
                    {
                        observation.stamp
                    } else {
                        transition.request.watch
                    }
                },
            ));
            self.state = ProductState::ErrorWithoutActive {
                failures: transition.failures,
            };
            return;
        }
        let incumbent = match transition.step {
            Step::Validating { incumbent } => incumbent,
            _ => None,
        };
        if let Some(active) = incumbent {
            self.state = match transition.origin {
                Origin::Restored(origin) => ProductState::ErrorWithActiveRestored {
                    active,
                    failed_candidate: origin.failed_candidate,
                    failures: origin.failures,
                },
                _ => ProductState::Active(active),
            };
        } else if transition.failures.incumbent.is_some()
            || self.owned_attempt.is_some()
            || !matches!(self.cleanup, CleanupStatus::Complete)
        {
            self.state = ProductState::ErrorWithoutActive {
                failures: transition.failures,
            };
        } else {
            self.state = match transition.origin {
                Origin::Error(failures) => ProductState::ErrorWithoutActive {
                    failures: *failures,
                },
                _ => ProductState::Stopped,
            };
        }
    }
    fn open_transition(
        &mut self,
        mut transition: Transition,
        recovering: bool,
    ) -> Option<ModelEffect> {
        let value = self.next_attempt.checked_add(1)?;
        let attempt = AttemptId::new(value)?;
        self.next_attempt = value;
        let key = AttemptKey {
            apply: transition.request.key.apply,
            attempt,
            purpose: transition.request.key.purpose,
        };
        transition.step = Step::Opening { key };
        let request = transition.request.clone();
        if key.purpose == AttemptPurpose::Resume {
            // Only the fresh open, after incumbent retirement, commits the
            // explicit live intent. Prevalidation refusal remains paused, and
            // a frozen loss still wins over this fallback during recovery.
            self.last_playback = request.playback;
        }
        self.owned_attempt = Some(attempt);
        self.put_transition(transition, recovering);
        Some(ModelEffect::Open { key, request })
    }
    pub fn open_verified(&mut self, key: AttemptKey) {
        if self.opening().map(|(current, _)| current) != Some(key) {
            return;
        }
        let Some((transition, recovering)) = self.take_transition() else {
            return;
        };
        let applied = AppliedSettings {
            settings: transition.request.settings,
        };
        let active = Active {
            applied: applied.clone(),
            key,
            playback: match transition.request.playback {
                InitialPlayback::Live => PlaybackState::Live,
                InitialPlayback::Paused => PlaybackState::Paused,
            },
            initial_playback: transition.request.playback,
        };
        self.last_valid = Some(applied);
        self.last_playback = transition.request.playback;
        self.recovery_apply = None;
        self.recovery = None;
        self.recovery_candidates.clear();
        self.loss_attempt = None;
        self.recovery_attempted = None;
        self.cleanup = CleanupStatus::Complete;
        self.state = if recovering && key.purpose == AttemptPurpose::Restore {
            // Candidate diagnostic remains attached to the submitted (still unapplied) draft.
            let failed_candidate = transition.failures.candidate.as_ref().map_or_else(
                || self.draft.settings.clone(),
                |failure| failure.requested.as_ref().clone(),
            );
            ProductState::ErrorWithActiveRestored {
                active,
                failed_candidate,
                failures: transition.failures,
            }
        } else {
            self.completed_failures = if transition.failures == FailureReport::default() {
                None
            } else {
                Some(transition.failures)
            };
            ProductState::Active(active)
        };
    }
    pub fn open_failed(&mut self, key: AttemptKey, failure: ApplyFailure) -> Option<ModelEffect> {
        if self.opening().map(|(current, _)| current) != Some(key) {
            // A real backend failure may follow EOF in the same terminal batch.
            // Upgrade its stage without issuing a second stop.
            if let ProductState::Applying(transition) | ProductState::Recovering(transition) =
                &mut self.state
                && matches!(transition.step, Step::Cleaning { key: current } if current == key)
            {
                match key.purpose {
                    AttemptPurpose::Restore => transition.failures.restore = Some(failure),
                    AttemptPurpose::Resume => transition.failures.resume = Some(failure),
                    _ => transition.failures.candidate = Some(failure),
                }
            }
            return None;
        }
        let (mut transition, recovering) = self.take_transition()?;
        match key.purpose {
            AttemptPurpose::Restore => transition.failures.restore = Some(failure),
            AttemptPurpose::Resume => transition.failures.resume = Some(failure),
            AttemptPurpose::Candidate | AttemptPurpose::Reconnect | AttemptPurpose::Recovery => {
                transition.failures.candidate = Some(failure);
            }
        }
        if matches!(
            key.purpose,
            AttemptPurpose::Recovery | AttemptPurpose::Reconnect
        ) {
            self.recovery_attempted = Some(self.observation.as_ref().map_or(
                transition.request.watch,
                |observation| {
                    if observation.stamp.watch == transition.request.watch.watch
                        && observation.stamp.epoch.get() > transition.request.watch.epoch.get()
                    {
                        observation.stamp
                    } else {
                        transition.request.watch
                    }
                },
            ));
        }
        transition.step = Step::Cleaning { key };
        self.cleanup = CleanupStatus::Draining;
        self.put_transition(transition, recovering);
        Some(ModelEffect::Stop {
            attempt: key.attempt,
            reason: StopIntent::Failed,
        })
    }
    pub fn session_failed(
        &mut self,
        attempt: AttemptId,
        failure: ApplyFailure,
    ) -> Option<ModelEffect> {
        if self.loss_attempt == Some(attempt) {
            if let Some(loss) = &mut self.recovery {
                loss.failure = failure.clone();
            }
            match &mut self.state {
                ProductState::Applying(transition) | ProductState::Recovering(transition) => {
                    transition.failures.incumbent = Some(failure);
                }
                _ => {
                    self.completed_failures
                        .get_or_insert_with(FailureReport::default)
                        .incumbent = Some(failure);
                }
            }
            return None;
        }
        if self.owned_attempt != Some(attempt) {
            return None;
        }
        if let Some((key, _)) = self.opening() {
            return self.open_failed(key, failure);
        }
        if self.active().map(Active::attempt) != Some(attempt) {
            return None;
        }
        self.prepared_pause = None;
        self.cleanup = CleanupStatus::Draining;
        if let Some((mut transition, recovering)) = self.take_transition() {
            if let Step::Validating { incumbent } = &mut transition.step {
                *incumbent = None;
            }
            transition.failures.incumbent = Some(failure);
            self.put_transition(transition, recovering);
        } else {
            let mut failures = self.failures().cloned().unwrap_or_default();
            failures.incumbent = Some(failure);
            self.state = ProductState::ErrorWithoutActive { failures };
        }
        Some(ModelEffect::Stop {
            attempt,
            reason: StopIntent::Failed,
        })
    }
    pub fn cleanup_error(&mut self, attempt: AttemptId, failure: ApplyFailure) {
        if self.owned_attempt != Some(attempt) {
            return;
        }
        let report = match &mut self.state {
            ProductState::Applying(transition) | ProductState::Recovering(transition) => {
                Some(&mut transition.failures)
            }
            ProductState::ErrorWithoutActive { failures } => Some(failures),
            ProductState::Disconnected
            | ProductState::SelectionRequired
            | ProductState::Stopping { .. } => Some(
                self.completed_failures
                    .get_or_insert_with(FailureReport::default),
            ),
            _ => None,
        };
        if let Some(report) = report {
            // Two prior unpoisoned owner outcomes, then at most cancellation,
            // two terminal blockers and the final owner outcome. Exact duplicate
            // callbacks are evidence of the same failure, not additional causes.
            if report.cleanup.len() < 6 && !report.cleanup.contains(&failure) {
                report.cleanup.push(failure);
            }
        }
    }
    pub fn cleanup_blocked(&mut self, attempt: AttemptId, failure: ApplyFailure) {
        if self.owned_attempt != Some(attempt) {
            return;
        }
        self.cleanup_error(attempt, failure.clone());
        self.cleanup = CleanupStatus::Blocked { failure };
        if !matches!(self.state, ProductState::Stopping { .. }) {
            let failures = self.failures().cloned().unwrap_or_default();
            self.state = ProductState::ErrorWithoutActive { failures };
        }
    }
    pub fn barrier_complete(&mut self, attempt: AttemptId) -> Option<ModelEffect> {
        if self.owned_attempt != Some(attempt) {
            return None;
        }
        self.owned_attempt = None;
        // Physical retirement does not authorize reopening a poisoned native
        // lifetime. Keep the failure policy, but release the actual resource lease.
        if matches!(self.cleanup, CleanupStatus::Blocked { .. }) {
            return None;
        }
        self.cleanup = CleanupStatus::Complete;
        let (mut transition, recovering) = self.take_transition()?;
        match transition.step {
            Step::ClosingOld => self.open_transition(transition, recovering),
            Step::Cleaning { .. }
                if !recovering && transition.request.key.purpose == AttemptPurpose::Candidate =>
            {
                let Some(prior) = &transition.prior else {
                    self.state = ProductState::ErrorWithoutActive {
                        failures: transition.failures,
                    };
                    return None;
                };
                let settings = prior.settings.clone();
                let watch = match self.ensure_watch(&settings) {
                    Ok(watch) => watch,
                    Err(error) => {
                        transition.failures.restore = Some(ApplyFailure::new(
                            crate::domain::failure::FailureCategory::Lifecycle(
                                crate::domain::failure::LifecycleFailure::Protocol,
                            ),
                            crate::domain::failure::Stage::Unknown,
                            crate::domain::failure::Cause::Generic,
                            settings,
                            "restore_watch",
                            error.to_string(),
                        ));
                        self.state = ProductState::ErrorWithoutActive {
                            failures: transition.failures,
                        };
                        return None;
                    }
                };
                transition.request = ValidationRequest {
                    key: ValidationKey {
                        apply: transition.request.key.apply,
                        purpose: AttemptPurpose::Restore,
                    },
                    revision: transition.request.revision,
                    settings,
                    playback: self
                        .recovery
                        .as_ref()
                        .map_or(self.last_playback, |loss| loss.playback),
                    watch,
                    choice: None,
                };
                transition.requested_target = transition.request.settings.clone();
                transition.step = Step::Validating { incumbent: None };
                let effect = ModelEffect::Validate(transition.request.clone());
                self.put_transition(transition, true);
                Some(effect)
            }
            Step::Cleaning { .. } => {
                self.state = ProductState::ErrorWithoutActive {
                    failures: transition.failures,
                };
                None
            }
            _ => {
                self.put_transition(transition, recovering);
                None
            }
        }
    }
    pub fn close(
        &mut self,
        expected: StateIdentity,
    ) -> Result<Option<ModelEffect>, CommandRejection> {
        if self.quitting {
            return Err(CommandRejection::ShuttingDown);
        }
        if expected != self.state_identity() {
            return Err(CommandRejection::StaleState);
        }
        Ok(self.stop(StopTarget::Session))
    }
    pub fn quit(&mut self) -> Option<ModelEffect> {
        if self.shutdown_ready() {
            return None;
        }
        self.quitting = true;
        self.stop(StopTarget::Quit)
    }
    fn stop(&mut self, target: StopTarget) -> Option<ModelEffect> {
        self.prepared_pause = None;
        self.recovery_apply = None;
        self.recovery = None;
        self.recovery_candidates.clear();
        self.recovery_attempted = None;
        self.loss_attempt = None;
        self.watch_target = None;
        self.observation = None;
        self.consumed_removal = None;
        if matches!(
            self.state,
            ProductState::Applying(_)
                | ProductState::Recovering(_)
                | ProductState::ErrorWithActiveRestored { .. }
                | ProductState::ErrorWithoutActive { .. }
        ) && let Some(failures) = self.failures().cloned()
            && failures != FailureReport::default()
        {
            self.completed_failures = Some(failures);
        }
        self.state = ProductState::Stopping { target };
        if let Some(attempt) = self.owned_attempt {
            if !matches!(self.cleanup, CleanupStatus::Blocked { .. }) {
                self.cleanup = CleanupStatus::Draining;
            }
            Some(ModelEffect::Stop {
                attempt,
                reason: if target == StopTarget::Quit {
                    StopIntent::Quit
                } else {
                    StopIntent::Close
                },
            })
        } else {
            None
        }
    }
    pub fn drain_complete(&mut self, validation_drained: bool, worker_retired: bool) {
        if self.owned_attempt.is_some()
            || !validation_drained
            || matches!(self.cleanup, CleanupStatus::Draining)
        {
            return;
        }
        if let ProductState::Stopping { target } = self.state {
            self.state = match target {
                StopTarget::Session if matches!(self.cleanup, CleanupStatus::Complete) => {
                    ProductState::Stopped
                }
                StopTarget::Session => ProductState::ErrorWithoutActive {
                    failures: self.completed_failures.take().unwrap_or_default(),
                },
                StopTarget::Quit if worker_retired => ProductState::ShutdownReady,
                StopTarget::Quit => return,
            };
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationRejection {
    pub request: ValidationRequest,
    pub failure: ApplyFailure,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capture::{
        CaptureMode, CapturedFourCc, DeviceIdentity, FrameRate, FrameSize, UsbTopology,
    };

    pub(super) fn settings(rate: u32) -> DraftSettings {
        DraftSettings {
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
                    rate: FrameRate::new(rate, 1).unwrap(),
                },
            },
            audio: AudioSelection::default(),
        }
    }

    #[test]
    fn zero_ids_rejected_and_revision_starts_zero() {
        assert!(ApplyId::new(0).is_none());
        assert!(AttemptId::new(0).is_none());
        assert_eq!(ProductModel::new(settings(60)).draft().revision.get(), 0);
    }

    #[test]
    fn overlapping_apply_and_stale_revision_preserve_frozen_candidate() {
        let mut model = ProductModel::new(settings(60));
        let expected = model.state_identity();
        let (apply, effect) = model.apply(expected, DraftRevision::new(0)).unwrap();
        assert!(
            matches!(&effect, ModelEffect::Validate(request) if request.settings == settings(60))
        );
        assert_eq!(
            model.apply(model.state_identity(), DraftRevision::new(0)),
            Err(CommandRejection::ApplyInProgress)
        );
        assert_eq!(
            model.edit_draft(DraftRevision::new(1), settings(30)),
            Err(CommandRejection::StaleRevision)
        );
        model
            .edit_draft(DraftRevision::new(0), settings(30))
            .unwrap();
        assert_eq!(model.validation_request().unwrap().key.apply, apply);
        assert_eq!(model.validation_request().unwrap().settings, settings(60));
        assert_eq!(model.draft().settings, settings(30));
    }

    #[test]
    fn counter_exhaustion_rejected_before_validation_or_teardown() {
        let mut model = ProductModel::new(settings(60));
        model.next_apply = u64::MAX;
        assert_eq!(
            model.apply(model.state_identity(), model.draft().revision),
            Err(CommandRejection::CounterExhausted)
        );
        assert_eq!(model.phase(), ProductPhase::Stopped);
        model.next_apply = 0;
        model.next_attempt = u64::MAX;
        assert_eq!(
            model.apply(model.state_identity(), model.draft().revision),
            Err(CommandRejection::CounterExhausted)
        );
        model.draft.revision = DraftRevision::new(u64::MAX);
        assert_eq!(
            model.edit_draft(model.draft().revision, settings(30)),
            Err(CommandRejection::CounterExhausted)
        );
    }
    fn initial_active(model: &mut ProductModel) -> AttemptKey {
        model
            .apply(model.state_identity(), model.draft().revision)
            .unwrap();
        let request = model.validation_request().unwrap().clone();
        let Some(ModelEffect::Open { key, .. }) = model.validation_succeeded(&request) else {
            panic!("initial open effect missing");
        };
        model.open_verified(key);
        key
    }

    #[test]
    fn successful_commit_never_overwrites_newer_draft_and_only_commit_constructs_last_valid() {
        let mut model = ProductModel::new(settings(60));
        model
            .apply(model.state_identity(), model.draft().revision)
            .unwrap();
        let request = model.validation_request().unwrap().clone();
        model
            .edit_draft(model.draft().revision, settings(30))
            .unwrap();
        assert!(model.last_valid().is_none());
        let Some(ModelEffect::Open { key, .. }) = model.validation_succeeded(&request) else {
            panic!("open missing");
        };
        model.open_verified(key);
        assert_eq!(model.last_valid().unwrap().settings(), &settings(60));
        assert_eq!(model.draft().settings, settings(30));
        assert_eq!(model.active().unwrap().attempt(), key.attempt);
    }

    #[test]
    fn physical_counter_reserves_one_possible_restore_before_touching_live_stream() {
        let mut model = ProductModel::new(settings(60));
        let old = initial_active(&mut model);
        model.next_attempt = u64::MAX - 1;
        assert_eq!(
            model.apply(model.state_identity(), model.draft().revision),
            Err(CommandRejection::CounterExhausted)
        );
        assert_eq!(model.active().unwrap().attempt(), old.attempt);
        assert_eq!(model.phase(), ProductPhase::Active);
        assert!(model.validation_request().is_none());
    }

    #[test]
    fn stale_state_rejected_and_quit_latched_even_when_no_resources_exist() {
        let mut model = ProductModel::new(settings(60));
        let stopped = model.state_identity();
        initial_active(&mut model);
        assert_eq!(
            model.apply(stopped, model.draft().revision),
            Err(CommandRejection::StaleState)
        );
        let old = model.active().unwrap().attempt();
        let _ = model.quit();
        let _ = model.barrier_complete(old);
        model.drain_complete(true, false);
        assert!(!model.shutdown_ready());
        model.drain_complete(true, true);
        assert!(model.shutdown_ready());
        assert_eq!(
            model.edit_draft(model.draft().revision, settings(30)),
            Err(CommandRejection::ShuttingDown)
        );
        assert_eq!(
            model.reconnect(model.state_identity()),
            Err(CommandRejection::ShuttingDown)
        );
    }

    fn playback_failure(category: crate::domain::failure::FailureCategory) -> ApplyFailure {
        ApplyFailure::new(
            category,
            crate::domain::failure::Stage::Unknown,
            crate::domain::failure::Cause::Generic,
            settings(60),
            "playback_test",
            "playback failure",
        )
    }

    fn paused_active(model: &mut ProductModel) -> AttemptKey {
        let key = initial_active(model);
        let request = model.prepare_pause(key.attempt).unwrap();
        assert!(model.pause_admitted(key.attempt, request));
        assert!(model.pause_observed(key.attempt, request, true));
        key
    }

    fn resume_open(model: &mut ProductModel, old: AttemptId) -> AttemptKey {
        let (_, effect) = model.resume(model.state_identity(), old).unwrap();
        let ModelEffect::Validate(request) = effect else {
            panic!("resume validation missing");
        };
        assert_eq!(
            model.validation_succeeded(&request),
            Some(ModelEffect::Stop {
                attempt: old,
                reason: StopIntent::Replace,
            })
        );
        let Some(ModelEffect::Open { key, .. }) = model.barrier_complete(old) else {
            panic!("resume open missing after retirement proof");
        };
        key
    }

    fn restored_active(model: &mut ProductModel) -> AttemptKey {
        let old = initial_active(model);
        model
            .apply(model.state_identity(), model.draft().revision)
            .unwrap();
        let request = model.validation_request().unwrap().clone();
        let _ = model.validation_succeeded(&request);
        let Some(ModelEffect::Open { key, .. }) = model.barrier_complete(old.attempt) else {
            panic!("candidate open missing");
        };
        let _ = model.open_failed(
            key,
            playback_failure(crate::domain::failure::FailureCategory::Session),
        );
        let Some(ModelEffect::Validate(request)) = model.barrier_complete(key.attempt) else {
            panic!("restore validation missing");
        };
        let Some(ModelEffect::Open { key, .. }) = model.validation_succeeded(&request) else {
            panic!("restore open missing");
        };
        model.open_verified(key);
        key
    }

    #[test]
    fn pause_reservation_and_admission_wait_for_exact_true_readback() {
        let mut model = ProductModel::new(settings(60));
        let key = initial_active(&mut model);
        let live = model.state_identity();
        let request = model.prepare_pause(key.attempt).unwrap();
        assert_eq!(model.state_identity(), live);
        assert_eq!(model.active().unwrap().playback(), PlaybackState::Live);
        assert!(!model.pause_observed(key.attempt, request, true));
        assert!(model.pause_admitted(key.attempt, request));
        let pending = model.state_identity();
        assert_ne!(pending, live);
        assert_eq!(model.phase(), ProductPhase::PausePending);
        assert_eq!(
            model.active().unwrap().playback(),
            PlaybackState::PausePending { request }
        );
        assert!(!model.pause_admitted(key.attempt, request));
        assert!(!model.pause_observed(AttemptId::new(999).unwrap(), request, true));
        assert!(!model.pause_observed(key.attempt, PauseRequestId::new(999).unwrap(), true));
        assert!(!model.pause_observed(key.attempt, request, false));
        assert_eq!(model.state_identity(), pending);
        assert!(model.pause_observed(key.attempt, request, true));
        assert_eq!(model.phase(), ProductPhase::Paused);
        assert_eq!(model.active().unwrap().playback(), PlaybackState::Paused);
        assert_ne!(model.state_identity(), pending);
        assert!(!model.pause_observed(key.attempt, request, true));
        assert!(!model.pause_observed(key.attempt, request, false));
    }

    #[test]
    fn pause_admission_requires_latest_reservation_for_the_exact_live_attempt() {
        let mut model = ProductModel::new(settings(60));
        let key = initial_active(&mut model);
        let first = model.prepare_pause(key.attempt).unwrap();
        let second = model.prepare_pause(key.attempt).unwrap();
        assert!(second.get() > first.get());
        assert!(!model.pause_admitted(key.attempt, first));
        assert!(!model.pause_admitted(AttemptId::new(999).unwrap(), second));
        assert!(!model.pause_admitted(key.attempt, PauseRequestId::new(999).unwrap()));
        assert_eq!(model.phase(), ProductPhase::Active);
        assert!(model.can_apply());
        assert!(model.pause_admitted(key.attempt, second));
    }

    #[test]
    fn pause_pending_rejects_playback_apply_and_restart_but_allows_draft_edits() {
        let mut model = ProductModel::new(settings(60));
        let key = initial_active(&mut model);
        let request = model.prepare_pause(key.attempt).unwrap();
        assert!(model.pause_admitted(key.attempt, request));
        let pending = model.state_identity();
        assert!(!model.can_apply());
        assert!(!model.can_restart());
        assert_eq!(
            model.prepare_pause(key.attempt),
            Err(CommandRejection::PlaybackBusy)
        );
        assert_eq!(
            model.apply(pending, model.draft().revision),
            Err(CommandRejection::PlaybackBusy)
        );
        assert_eq!(model.restart(pending), Err(CommandRejection::PlaybackBusy));
        assert_eq!(
            model.resume(pending, key.attempt),
            Err(CommandRejection::PlaybackBusy)
        );
        model
            .edit_draft(model.draft().revision, settings(30))
            .unwrap();
        assert_eq!(model.state_identity(), pending);
        assert_eq!(model.draft().settings, settings(30));
    }

    #[test]
    fn playback_commands_reject_unavailable_stale_and_exhausted_state_without_mutation() {
        assert!(PauseRequestId::new(0).is_none());
        let mut model = ProductModel::new(settings(60));
        let unknown = AttemptId::new(999).unwrap();
        assert_eq!(
            model.prepare_pause(unknown),
            Err(CommandRejection::PlaybackUnavailable)
        );
        assert_eq!(
            model.resume(model.state_identity(), unknown),
            Err(CommandRejection::PlaybackUnavailable)
        );
        let key = initial_active(&mut model);
        let live = model.state_identity();
        assert_eq!(
            model.prepare_pause(unknown),
            Err(CommandRejection::StaleState)
        );
        assert_eq!(
            model.resume(live, key.attempt),
            Err(CommandRejection::PlaybackUnavailable)
        );
        model.next_pause = u64::MAX;
        assert_eq!(
            model.prepare_pause(key.attempt),
            Err(CommandRejection::CounterExhausted)
        );
        assert_eq!(model.state_identity(), live);
        model.next_pause = 0;
        let request = model.prepare_pause(key.attempt).unwrap();
        assert!(model.pause_admitted(key.attempt, request));
        assert!(model.pause_observed(key.attempt, request, true));
        assert_eq!(
            model.resume(live, key.attempt),
            Err(CommandRejection::StaleState)
        );
        assert_eq!(
            model.resume(model.state_identity(), unknown),
            Err(CommandRejection::StaleState)
        );
        assert_eq!(
            model.prepare_pause(key.attempt),
            Err(CommandRejection::PlaybackUnavailable)
        );
    }

    #[test]
    fn resume_opens_applied_settings_once_after_matching_retirement_not_newer_draft() {
        let mut model = ProductModel::new(settings(60));
        let old = paused_active(&mut model);
        let revision = model
            .edit_draft(model.draft().revision, settings(30))
            .unwrap();
        let (apply, effect) = model.resume(model.state_identity(), old.attempt).unwrap();
        let ModelEffect::Validate(request) = effect else {
            panic!("resume validation missing");
        };
        assert_eq!(request.key.purpose, AttemptPurpose::Resume);
        assert_eq!(request.settings, settings(60));
        assert_eq!(model.phase(), ProductPhase::ValidatingResume);
        assert_eq!(model.active().unwrap().playback(), PlaybackState::Paused);
        assert_eq!(
            model.validation_succeeded(&request),
            Some(ModelEffect::Stop {
                attempt: old.attempt,
                reason: StopIntent::Replace,
            })
        );
        assert_eq!(model.phase(), ProductPhase::ClosingResume);
        assert!(model.active().is_none());
        assert!(model.opening().is_none());
        assert_eq!(model.barrier_complete(AttemptId::new(999).unwrap()), None);
        assert_eq!(model.validation_succeeded(&request), None);
        let Some(ModelEffect::Open { key, request }) = model.barrier_complete(old.attempt) else {
            panic!("resume open missing");
        };
        assert_eq!(key.apply, apply);
        assert_eq!(key.purpose, AttemptPurpose::Resume);
        assert_ne!(key.attempt, old.attempt);
        assert_eq!(request.settings, settings(60));
        assert_eq!(model.phase(), ProductPhase::OpeningResume);
        assert_eq!(model.barrier_complete(old.attempt), None);
        model.open_verified(old);
        assert_eq!(model.phase(), ProductPhase::OpeningResume);
        model.open_verified(key);
        assert_eq!(model.phase(), ProductPhase::Active);
        assert_eq!(model.active().unwrap().playback(), PlaybackState::Live);
        assert_eq!(model.active().unwrap().applied().settings(), &settings(60));
        assert_eq!(model.last_valid().unwrap().settings(), &settings(60));
        assert_eq!(model.draft().revision, revision);
        assert_eq!(model.draft().settings, settings(30));
    }

    #[test]
    fn resume_prevalidation_rejection_preserves_paused_incumbent_and_restored_origin() {
        for restored in [false, true] {
            let mut model = ProductModel::new(settings(60));
            let key = if restored {
                restored_active(&mut model)
            } else {
                initial_active(&mut model)
            };
            let request = model.prepare_pause(key.attempt).unwrap();
            let before = model.state_identity();
            assert!(model.pause_admitted(key.attempt, request));
            assert!(model.pause_observed(key.attempt, request, true));
            let paused = model.state_identity();
            assert_ne!(paused, before);
            assert_eq!(
                model.phase(),
                if restored {
                    ProductPhase::ErrorWithActiveRestored
                } else {
                    ProductPhase::Paused
                }
            );
            let active = model.active().unwrap().clone();
            let prior = model.last_valid().cloned();
            let failures = model.failures().cloned();
            let (_, effect) = model.resume(paused, key.attempt).unwrap();
            let ModelEffect::Validate(request) = effect else {
                panic!("resume validation missing");
            };
            let failure = playback_failure(crate::domain::failure::FailureCategory::Validation(
                crate::domain::failure::ValidationLayer::Mode,
            ));
            model.validation_failed(request.clone(), failure.clone());
            assert_eq!(model.active(), Some(&active));
            assert_eq!(model.last_valid(), prior.as_ref());
            assert_eq!(model.failures(), failures.as_ref());
            assert_eq!(model.cleanup(), &CleanupStatus::Complete);
            assert_eq!(
                model.validation_rejection(),
                Some(&ValidationRejection { request, failure })
            );
            assert!(model.opening().is_none());
        }
    }

    #[test]
    fn incumbent_failure_during_resume_validation_never_resurrects_paused_session() {
        for valid in [false, true] {
            for retired_before_validation in [false, true] {
                let mut model = ProductModel::new(settings(60));
                let old = paused_active(&mut model);
                model.resume(model.state_identity(), old.attempt).unwrap();
                let request = model.validation_request().unwrap().clone();
                let failure = playback_failure(crate::domain::failure::FailureCategory::Session);
                assert_eq!(
                    model.session_failed(old.attempt, failure.clone()),
                    Some(ModelEffect::Stop {
                        attempt: old.attempt,
                        reason: StopIntent::Failed,
                    })
                );
                assert!(model.active().is_none());
                if retired_before_validation {
                    assert_eq!(model.barrier_complete(old.attempt), None);
                }
                let effect = if valid {
                    model.validation_succeeded(&request)
                } else {
                    model.validation_failed(
                        request,
                        playback_failure(crate::domain::failure::FailureCategory::Validation(
                            crate::domain::failure::ValidationLayer::Mode,
                        )),
                    );
                    None
                };
                assert!(model.active().is_none());
                assert_eq!(model.failures().unwrap().incumbent.as_ref(), Some(&failure));
                if valid && retired_before_validation {
                    assert!(matches!(effect, Some(ModelEffect::Open { .. })));
                } else {
                    assert_eq!(effect, None);
                }
                if !retired_before_validation {
                    let effect = model.barrier_complete(old.attempt);
                    assert_eq!(matches!(effect, Some(ModelEffect::Open { .. })), valid);
                }
                assert_eq!(
                    model.phase(),
                    if valid {
                        ProductPhase::OpeningResume
                    } else {
                        ProductPhase::ErrorWithoutActive
                    }
                );
            }
        }
    }

    #[test]
    fn terminal_session_failure_outranks_pending_and_late_pause_observations() {
        for confirmed in [false, true] {
            let mut model = ProductModel::new(settings(60));
            let key = initial_active(&mut model);
            let request = model.prepare_pause(key.attempt).unwrap();
            assert!(model.pause_admitted(key.attempt, request));
            if confirmed {
                assert!(model.pause_observed(key.attempt, request, true));
            }
            let _ = model.session_failed(
                key.attempt,
                playback_failure(crate::domain::failure::FailureCategory::Session),
            );
            assert!(!model.pause_observed(key.attempt, request, true));
            assert!(!model.pause_admitted(key.attempt, request));
            assert!(model.active().is_none());
            assert_eq!(model.phase(), ProductPhase::ErrorWithoutActive);
            assert!(!model.can_reconnect());
            assert_eq!(model.barrier_complete(key.attempt), None);
            assert!(model.can_reconnect());
            let (_, effect) = model.reconnect(model.state_identity()).unwrap();
            let Some(ModelEffect::Validate(validation)) = effect else {
                panic!("paused reconnect validation missing");
            };
            assert_eq!(validation.playback, InitialPlayback::Paused);
            let Some(ModelEffect::Open { key, .. }) = model.validation_succeeded(&validation)
            else {
                panic!("paused reconnect open missing");
            };
            model.open_verified(key);
            assert_eq!(model.active().unwrap().playback(), PlaybackState::Paused);
            assert_eq!(
                model.active().unwrap().initial_playback(),
                InitialPlayback::Paused
            );
        }
    }

    #[test]
    fn stable_paused_apply_and_restart_preserve_pause_on_rejection_then_replace_live() {
        for restart in [false, true] {
            let mut model = ProductModel::new(settings(60));
            let old = paused_active(&mut model);
            model
                .edit_draft(model.draft().revision, settings(30))
                .unwrap();
            assert!(model.can_apply());
            assert!(model.can_restart());
            for valid in [false, true] {
                let expected = model.state_identity();
                let (_, effect) = if restart {
                    model.restart(expected).unwrap()
                } else {
                    model.apply(expected, model.draft().revision).unwrap()
                };
                let ModelEffect::Validate(request) = effect else {
                    panic!("replacement validation missing");
                };
                if !valid {
                    model.validation_failed(
                        request,
                        playback_failure(crate::domain::failure::FailureCategory::Validation(
                            crate::domain::failure::ValidationLayer::Mode,
                        )),
                    );
                    assert_eq!(model.active().unwrap().attempt(), old.attempt);
                    assert_eq!(model.active().unwrap().playback(), PlaybackState::Paused);
                    continue;
                }
                let _ = model.validation_succeeded(&request);
                let Some(ModelEffect::Open { key, .. }) = model.barrier_complete(old.attempt)
                else {
                    panic!("replacement open missing");
                };
                model.open_verified(key);
                assert_eq!(model.active().unwrap().playback(), PlaybackState::Live);
                assert_eq!(
                    model.active().unwrap().applied().settings(),
                    &settings(if restart { 60 } else { 30 })
                );
            }
        }
    }

    #[test]
    fn resume_open_failure_waits_retirement_and_never_automatically_restores() {
        let mut model = ProductModel::new(settings(60));
        let old = paused_active(&mut model);
        let key = resume_open(&mut model, old.attempt);
        assert_eq!(
            model.opening_request().unwrap().playback,
            InitialPlayback::Live
        );
        let failure = playback_failure(crate::domain::failure::FailureCategory::Session);
        assert_eq!(
            model.open_failed(key, failure.clone()),
            Some(ModelEffect::Stop {
                attempt: key.attempt,
                reason: StopIntent::Failed,
            })
        );
        assert_eq!(model.phase(), ProductPhase::CleaningFailedResume);
        assert_eq!(model.cleanup(), &CleanupStatus::Draining);
        assert_eq!(model.failures().unwrap().resume.as_ref(), Some(&failure));
        assert!(model.failures().unwrap().candidate.is_none());
        assert!(model.failures().unwrap().restore.is_none());
        assert!(!model.can_reconnect());
        assert_eq!(model.barrier_complete(old.attempt), None);
        model.open_verified(key);
        assert_eq!(model.phase(), ProductPhase::CleaningFailedResume);
        assert_eq!(model.barrier_complete(key.attempt), None);
        assert_eq!(model.barrier_complete(key.attempt), None);
        assert_eq!(model.phase(), ProductPhase::ErrorWithoutActive);
        assert_eq!(model.cleanup(), &CleanupStatus::Complete);
        assert_eq!(model.last_valid().unwrap().settings(), &settings(60));
        assert!(model.validation_request().is_none());
        assert!(model.opening().is_none());
        assert!(model.can_reconnect());
        let (_, effect) = model.reconnect(model.state_identity()).unwrap();
        let Some(ModelEffect::Validate(request)) = effect else {
            panic!("explicit reconnect validation missing");
        };
        assert_eq!(request.key.purpose, AttemptPurpose::Reconnect);
        assert_eq!(request.playback, InitialPlayback::Live);
        let Some(ModelEffect::Open { key: reconnect, .. }) = model.validation_succeeded(&request)
        else {
            panic!("explicit reconnect open missing");
        };
        model.open_verified(reconnect);
        assert_eq!(model.active().unwrap().playback(), PlaybackState::Live);
    }

    #[test]
    fn loss_during_resume_validation_keeps_frozen_pause_after_failed_live_open() {
        let mut model = ProductModel::new(settings(60));
        let old = paused_active(&mut model);
        model.resume(model.state_identity(), old.attempt).unwrap();
        let request = model.validation_request().unwrap().clone();
        assert_eq!(request.playback, InitialPlayback::Live);
        let failure = playback_failure(crate::domain::failure::FailureCategory::Session);
        assert_eq!(
            model.video_lost(
                old.attempt,
                LossEvidence::StreamEnded {
                    reason: 0,
                    error: 0
                },
                failure.clone(),
            ),
            Some(ModelEffect::Stop {
                attempt: old.attempt,
                reason: StopIntent::Failed,
            })
        );
        let loss = model.recovery().unwrap().clone();
        assert_eq!(loss.playback, InitialPlayback::Paused);
        assert_eq!(model.validation_succeeded(&request), None);
        let Some(ModelEffect::Open { key, request }) = model.barrier_complete(old.attempt) else {
            panic!("validated resume open missing after loss retirement");
        };
        assert_eq!(request.playback, InitialPlayback::Live);
        assert_eq!(
            model.open_failed(key, failure),
            Some(ModelEffect::Stop {
                attempt: key.attempt,
                reason: StopIntent::Failed,
            })
        );
        assert_eq!(model.barrier_complete(key.attempt), None);
        assert_eq!(model.recovery(), Some(&loss));
        assert!(model.validation_request().is_none());
        let (_, effect) = model.reconnect(model.state_identity()).unwrap();
        let Some(ModelEffect::Validate(request)) = effect else {
            panic!("loss reconnect validation missing");
        };
        assert_eq!(request.settings, settings(60));
        assert_eq!(request.playback, InitialPlayback::Paused);
        let Some(ModelEffect::Open { key, .. }) = model.validation_succeeded(&request) else {
            panic!("loss reconnect open missing");
        };
        model.open_verified(key);
        assert_eq!(model.active().unwrap().playback(), PlaybackState::Paused);
    }

    #[test]
    fn resume_no_resource_failure_completes_on_immediate_empty_retirement_proof() {
        let mut model = ProductModel::new(settings(60));
        let old = paused_active(&mut model);
        let key = resume_open(&mut model, old.attempt);
        let _ = model.open_failed(
            key,
            playback_failure(crate::domain::failure::FailureCategory::Lifecycle(
                crate::domain::failure::LifecycleFailure::OwnerSpawn,
            )),
        );
        // The app's StartFailure::NoResourcesCreated path has no physical
        // lifetime and supplies the empty barrier immediately.
        assert_eq!(model.barrier_complete(key.attempt), None);
        assert_eq!(model.phase(), ProductPhase::ErrorWithoutActive);
        assert!(model.can_reconnect());
        assert!(model.failures().unwrap().resume.is_some());
        assert!(model.validation_request().is_none());
    }

    #[test]
    fn resume_overlap_rejected_at_every_transition_without_changing_target() {
        let mut model = ProductModel::new(settings(60));
        let old = paused_active(&mut model);
        model.resume(model.state_identity(), old.attempt).unwrap();
        let request = model.validation_request().unwrap().clone();
        for step in 0..4 {
            let identity = model.state_identity();
            assert!(!model.can_apply());
            assert!(!model.can_restart());
            assert_eq!(
                model.apply(identity, model.draft().revision),
                Err(CommandRejection::ApplyInProgress)
            );
            assert_eq!(
                model.restart(identity),
                Err(CommandRejection::ApplyInProgress)
            );
            assert_eq!(
                model.resume(identity, old.attempt),
                Err(CommandRejection::ApplyInProgress)
            );
            assert_eq!(
                model.prepare_pause(old.attempt),
                Err(CommandRejection::ApplyInProgress)
            );
            assert_eq!(model.state_identity(), identity);
            model
                .edit_draft(model.draft().revision, settings(30))
                .unwrap();
            match step {
                0 => {
                    let _ = model.validation_succeeded(&request);
                }
                1 => {
                    let _ = model.barrier_complete(old.attempt);
                }
                2 => {
                    let key = model.opening().unwrap().0;
                    let _ = model.open_failed(
                        key,
                        playback_failure(crate::domain::failure::FailureCategory::Session),
                    );
                }
                _ => {}
            }
        }
    }

    #[test]
    fn close_and_quit_cancel_each_resume_step_and_drain_before_completion() {
        for quit in [false, true] {
            for step in 0..4 {
                let mut model = ProductModel::new(settings(60));
                let old = paused_active(&mut model);
                model.resume(model.state_identity(), old.attempt).unwrap();
                let request = model.validation_request().unwrap().clone();
                if step > 0 {
                    let _ = model.validation_succeeded(&request);
                }
                if step > 1 {
                    let _ = model.barrier_complete(old.attempt);
                }
                let opening = model.opening().map(|(key, _)| key);
                if step > 2 {
                    let _ = model.open_failed(
                        opening.unwrap(),
                        playback_failure(crate::domain::failure::FailureCategory::Session),
                    );
                }
                let owned = model.state_identity().attempt().unwrap();
                let effect = if quit {
                    model.quit()
                } else {
                    model.close(model.state_identity()).unwrap()
                };
                assert_eq!(
                    effect,
                    Some(ModelEffect::Stop {
                        attempt: owned,
                        reason: if quit {
                            StopIntent::Quit
                        } else {
                            StopIntent::Close
                        },
                    })
                );
                assert_eq!(model.validation_succeeded(&request), None);
                model.validation_failed(
                    request,
                    playback_failure(crate::domain::failure::FailureCategory::Session),
                );
                if let Some(key) = opening {
                    model.open_verified(key);
                }
                model.drain_complete(true, true);
                assert_eq!(model.phase(), ProductPhase::Stopping);
                assert_eq!(model.barrier_complete(AttemptId::new(999).unwrap()), None);
                assert_eq!(model.barrier_complete(owned), None);
                model.drain_complete(false, true);
                assert_eq!(model.phase(), ProductPhase::Stopping);
                model.drain_complete(true, false);
                assert_eq!(
                    model.phase(),
                    if quit {
                        ProductPhase::Stopping
                    } else {
                        ProductPhase::Stopped
                    }
                );
                model.drain_complete(true, true);
                assert_eq!(
                    model.phase(),
                    if quit {
                        ProductPhase::ShutdownReady
                    } else {
                        ProductPhase::Stopped
                    }
                );
                assert!(model.opening().is_none());
                assert!(model.active().is_none());
                if quit {
                    assert_eq!(
                        model.prepare_pause(owned),
                        Err(CommandRejection::ShuttingDown)
                    );
                    assert_eq!(
                        model.resume(model.state_identity(), owned),
                        Err(CommandRejection::ShuttingDown)
                    );
                }
            }
        }
    }

    #[test]
    fn resume_reserves_one_attempt_and_counter_exhaustion_preserves_paused_owner() {
        let mut model = ProductModel::new(settings(60));
        let old = paused_active(&mut model);
        let paused = model.state_identity();
        model.next_apply = u64::MAX;
        assert_eq!(
            model.resume(paused, old.attempt),
            Err(CommandRejection::CounterExhausted)
        );
        assert_eq!(model.state_identity(), paused);
        assert!(model.validation_request().is_none());
        model.next_apply = old.apply.get();
        model.next_attempt = u64::MAX;
        assert_eq!(
            model.resume(paused, old.attempt),
            Err(CommandRejection::CounterExhausted)
        );
        assert_eq!(model.state_identity(), paused);
        model.next_attempt = u64::MAX - 1;
        assert_eq!(
            model.restart(paused),
            Err(CommandRejection::CounterExhausted)
        );
        assert_eq!(model.state_identity(), paused);
        assert!(model.validation_request().is_none());
        let key = resume_open(&mut model, old.attempt);
        assert_eq!(key.attempt.get(), u64::MAX);
        assert_eq!(model.cutover_validation(key), None);
        model.open_verified(key);
        assert_eq!(model.active().unwrap().playback(), PlaybackState::Live);
        let request = model.prepare_pause(key.attempt).unwrap();
        assert!(model.pause_admitted(key.attempt, request));
        assert!(model.pause_observed(key.attempt, request, true));
        let paused = model.state_identity();
        assert_eq!(
            model.resume(paused, key.attempt),
            Err(CommandRejection::CounterExhausted)
        );
        assert_eq!(model.state_identity(), paused);
        assert_eq!(model.active().unwrap().attempt(), key.attempt);
        assert!(model.validation_request().is_none());
    }

    #[test]
    fn resume_with_different_watch_reserves_cutover_and_physical_open_before_retirement() {
        let mut model = ProductModel::new(settings(60));
        model
            .apply(model.state_identity(), model.draft().revision)
            .unwrap();
        let request = model.validation_request().unwrap().clone();
        let mut relocated = settings(60);
        relocated.video.identity = DeviceIdentity::new(
            0x32ed,
            0x3701,
            UsbTopology::new(
                "controller".into(),
                vec![std::num::NonZeroU8::new(2).unwrap()],
            )
            .unwrap(),
            Some("serial".into()),
        )
        .unwrap();
        let request = model
            .accept_prepared(&request, relocated.clone(), request.watch)
            .unwrap();
        let Some(ModelEffect::Open { key: old, .. }) = model.validation_succeeded(&request) else {
            panic!("relocated initial open missing");
        };
        model.open_verified(old);
        // Before the consumer installs committed_watch, the saved complete
        // identity differs from the watch that validated the original request.
        let pause = model.prepare_pause(old.attempt).unwrap();
        assert!(model.pause_admitted(old.attempt, pause));
        assert!(model.pause_observed(old.attempt, pause, true));
        let paused = model.state_identity();
        model.next_attempt = u64::MAX - 1;
        assert_eq!(
            model.resume(paused, old.attempt),
            Err(CommandRejection::CounterExhausted)
        );
        assert_eq!(model.state_identity(), paused);
        assert_eq!(model.active().unwrap().attempt(), old.attempt);
        assert!(model.validation_request().is_none());
        model.next_attempt = u64::MAX - 2;
        let reserved = resume_open(&mut model, old.attempt);
        assert_eq!(reserved.attempt.get(), u64::MAX - 1);
        let Some(ModelEffect::Validate(request)) = model.cutover_validation(reserved) else {
            panic!("fresh cutover validation missing");
        };
        assert_eq!(request.settings, relocated);
        assert_eq!(request.playback, InitialPlayback::Live);
        assert_eq!(model.barrier_complete(reserved.attempt), None);
        assert!(model.opening().is_none());
        let Some(ModelEffect::Open { key, .. }) = model.validation_succeeded(&request) else {
            panic!("physical resume open missing after cutover");
        };
        assert_eq!(key.attempt.get(), u64::MAX);
        assert_eq!(key.apply, reserved.apply);
        model.open_verified(reserved);
        assert_eq!(model.phase(), ProductPhase::OpeningResume);
        model.open_verified(key);
        assert_eq!(model.active().unwrap().attempt(), key.attempt);
        assert_eq!(model.active().unwrap().applied().settings(), &relocated);
        assert_eq!(model.active().unwrap().playback(), PlaybackState::Live);
    }

    #[test]
    fn replacement_and_close_invalidate_unadmitted_pause_reservations() {
        for close in [false, true] {
            let mut model = ProductModel::new(settings(60));
            let old = initial_active(&mut model);
            let pause = model.prepare_pause(old.attempt).unwrap();
            if close {
                let _ = model.close(model.state_identity()).unwrap();
            } else {
                model.restart(model.state_identity()).unwrap();
                let request = model.validation_request().unwrap().clone();
                model.validation_failed(
                    request,
                    playback_failure(crate::domain::failure::FailureCategory::Validation(
                        crate::domain::failure::ValidationLayer::Mode,
                    )),
                );
            }
            assert!(!model.pause_admitted(old.attempt, pause));
            assert!(!model.pause_observed(old.attempt, pause, true));
        }
    }

    #[test]
    fn poisoned_resume_retirement_never_reopens_and_quit_keeps_failure() {
        let mut model = ProductModel::new(settings(60));
        let old = paused_active(&mut model);
        model.resume(model.state_identity(), old.attempt).unwrap();
        let request = model.validation_request().unwrap().clone();
        let _ = model.validation_succeeded(&request);
        let failure = playback_failure(crate::domain::failure::FailureCategory::Lifecycle(
            crate::domain::failure::LifecycleFailure::SurfaceLoss,
        ));
        model.cleanup_blocked(old.attempt, failure.clone());
        assert_eq!(model.barrier_complete(old.attempt), None);
        assert!(!model.can_reconnect());
        assert_eq!(
            model.cleanup(),
            &CleanupStatus::Blocked {
                failure: failure.clone()
            }
        );
        assert_eq!(model.quit(), None);
        model.drain_complete(true, true);
        assert!(model.shutdown_ready());
        assert!(model.failures().unwrap().cleanup.contains(&failure));
    }

    #[test]
    fn close_and_quit_ignore_late_pause_admission_and_readback() {
        for quit in [false, true] {
            for step in 0..3 {
                let mut model = ProductModel::new(settings(60));
                let key = initial_active(&mut model);
                let request = model.prepare_pause(key.attempt).unwrap();
                if step > 0 {
                    assert!(model.pause_admitted(key.attempt, request));
                }
                if step > 1 {
                    assert!(model.pause_observed(key.attempt, request, true));
                }
                let effect = if quit {
                    model.quit()
                } else {
                    model.close(model.state_identity()).unwrap()
                };
                assert_eq!(
                    effect,
                    Some(ModelEffect::Stop {
                        attempt: key.attempt,
                        reason: if quit {
                            StopIntent::Quit
                        } else {
                            StopIntent::Close
                        },
                    })
                );
                assert!(!model.pause_admitted(key.attempt, request));
                assert!(!model.pause_observed(key.attempt, request, true));
                assert_eq!(model.barrier_complete(key.attempt), None);
                model.drain_complete(true, true);
                assert_eq!(
                    model.phase(),
                    if quit {
                        ProductPhase::ShutdownReady
                    } else {
                        ProductPhase::Stopped
                    }
                );
                assert!(model.active().is_none());
            }
        }
    }
}
