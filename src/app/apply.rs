//! Serialized product orchestration over opaque validation and session ports.

use crate::domain::{
    capture::{
        AudioAvailability, AudioEpoch, AudioSelection, AudioSilence, LossEvidence, PlaybackGain,
        RecoveryObservation, RecoveryWatchTarget, SelectionToken, SourcePresence, WatchStamp,
    },
    failure::{ApplyFailure, Cause, FailureCategory, LifecycleFailure, Stage, ValidationLayer},
    output::{OutputPlan, OutputRevision, OutputSilence},
    state::{
        AppliedSettings, ApplyId, AttemptId, AttemptKey, AttemptPurpose, CleanupStatus,
        CommandRejection, DraftRevision, DraftSettings, InitialPlayback, ModelEffect,
        PlaybackState, ProductModel, ProductPhase, ReconnectAdmission, StateIdentity, StopIntent,
        ValidationRequest,
    },
};

use super::ports::{
    AudioOutcome, DraftValidator, ImmediateIntent, OpenReadiness, OpenReceipt, SessionEvent,
    SessionRunner, StartFailure, StopReason, StopSubmission, SubmitFailure, SubmitStatus,
    ValidationOutcome,
};

/// An exact verified open, after committed health monitoring has been installed.
/// Its purpose alone does not identify a user Apply; consumers correlate the key
/// with their admitted application intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedOpen {
    pub key: AttemptKey,
    pub applied: AppliedSettings,
}

struct Lease {
    key: AttemptKey,
    settings: DraftSettings,
    owner_stopped: bool,
    stopping: bool,
    verified: bool,
    blocked: bool,
    watch: WatchStamp,
    playback: InitialPlayback,
    audio_epoch: Option<AudioEpoch>,
    last_audio_epoch: u64,
    audio_stamp: Option<WatchStamp>,
    audio_retired: bool,
    audio_detaching: bool,
    audio_attempted: Option<WatchStamp>,
    audio_terminal: bool,
    choice: Option<SelectionToken>,
}

pub struct ApplyCoordinator<V, R>
where
    V: DraftValidator,
    R: SessionRunner<Prepared = V::Prepared>,
{
    model: ProductModel,
    validator: V,
    runner: R,
    gain: PlaybackGain,
    output: OutputPlan,
    output_initialized: bool,
    validation: Option<ValidationRequest>,
    prepared: Option<(ValidationRequest, V::Prepared)>,
    lease: Option<Lease>,
    audio: Option<AudioAvailability>,
    validator_shutdown: bool,
    installed_watch: Option<RecoveryWatchTarget>,
    pending_validation: Option<ValidationRequest>,
    deferred_ready: Option<(AttemptKey, OpenReceipt)>,
    verified_open: Option<VerifiedOpen>,
    strict_startup_apply: Option<ApplyId>,
}
impl<V, R> ApplyCoordinator<V, R>
where
    V: DraftValidator,
    R: SessionRunner<Prepared = V::Prepared>,
{
    pub fn new(settings: DraftSettings, gain: PlaybackGain, validator: V, runner: R) -> Self {
        Self {
            model: ProductModel::new(settings),
            validator,
            runner,
            gain,
            output: OutputPlan::Silent {
                revision: OutputRevision::first(),
                reason: OutputSilence::CatalogUnavailable("output catalog not observed".into()),
            },
            output_initialized: false,
            validation: None,
            prepared: None,
            lease: None,
            audio: None,
            validator_shutdown: false,
            installed_watch: None,
            pending_validation: None,
            deferred_ready: None,
            verified_open: None,
            strict_startup_apply: None,
        }
    }
    pub fn model(&self) -> &ProductModel {
        &self.model
    }
    pub fn validator_mut(&mut self) -> &mut V {
        &mut self.validator
    }
    pub fn runner_mut(&mut self) -> &mut R {
        &mut self.runner
    }
    pub fn gain(&self) -> PlaybackGain {
        self.gain
    }
    pub fn audio_availability(&self) -> Option<&AudioAvailability> {
        self.audio.as_ref()
    }
    pub fn output_plan(&self) -> &OutputPlan {
        &self.output
    }

    /// Immediate preference/policy intent, separate from capture Draft/Applied.
    /// Idle and paused sessions retain it for the next fresh open.
    pub fn set_output_plan(&mut self, plan: OutputPlan) -> Result<SubmitStatus, CommandRejection> {
        self.check_drain()?;
        if self.model.phase() == ProductPhase::Stopping
            || self
                .lease
                .as_ref()
                .is_some_and(|lease| lease.stopping || lease.owner_stopped || lease.blocked)
        {
            return Ok(SubmitStatus::Closing);
        }
        if self.output_initialized
            && (plan.revision().get() < self.output.revision().get()
                || (plan.revision() == self.output.revision() && plan != self.output))
        {
            return Err(CommandRejection::StaleState);
        }
        if plan == self.output {
            self.output_initialized = true;
            return Ok(SubmitStatus::Accepted);
        }
        let status = self.lease.as_ref().map_or(SubmitStatus::Accepted, |lease| {
            self.runner
                .submit_immediate(lease.key.attempt, ImmediateIntent::SetOutput(plan.clone()))
        });
        if status == SubmitStatus::Accepted {
            self.output = plan;
            self.output_initialized = true;
            self.reconcile_audio();
        }
        Ok(status)
    }
    /// Consume the latest verified open once. Drain after each poll before
    /// admitting another operation: a later verified open replaces an untaken
    /// notification. Session loss or draft edits do not revoke a past success.
    pub fn take_verified_open(&mut self) -> Option<VerifiedOpen> {
        self.verified_open.take()
    }
    /// Startup restore requires the exact requested source to be healthy. A
    /// missing output may honestly keep verified video silent; source/transport
    /// failures still reject restoration rather than borrowing an active receipt.
    /// Arm immediately after admitting this initial Candidate, before polling.
    /// The guard cannot attach to an incumbent, rollback, or stale operation.
    pub fn require_startup_restore_audio(
        &mut self,
        apply: ApplyId,
    ) -> Result<(), CommandRejection> {
        self.check_drain()?;
        if !self.startup_restore_candidate(apply) {
            return Err(CommandRejection::StaleState);
        }
        self.strict_startup_apply = Some(apply);
        Ok(())
    }
    fn startup_restore_candidate(&self, apply: ApplyId) -> bool {
        self.model.last_valid().is_none()
            && (self.model.validation_request().is_some_and(|request| {
                request.key.apply == apply && request.key.purpose == AttemptPurpose::Candidate
            }) || self.model.opening().is_some_and(|(key, _)| {
                key.apply == apply && key.purpose == AttemptPurpose::Candidate
            }))
    }
    fn retire_startup_restore_guard(&mut self) {
        if self
            .strict_startup_apply
            .is_some_and(|apply| !self.startup_restore_candidate(apply))
        {
            self.strict_startup_apply = None;
        }
    }
    pub fn edit_draft(
        &mut self,
        expected: DraftRevision,
        settings: DraftSettings,
    ) -> Result<DraftRevision, CommandRejection> {
        self.model.edit_draft(expected, settings)
    }
    fn check_drain(&self) -> Result<(), CommandRejection> {
        if self.validator_shutdown {
            return Err(CommandRejection::ShuttingDown);
        }
        if self.validation.is_some() && self.model.validation_request().is_none() {
            return Err(CommandRejection::CleanupIncomplete);
        }
        Ok(())
    }
    pub fn apply(
        &mut self,
        expected_state: StateIdentity,
        expected_revision: DraftRevision,
    ) -> Result<ApplyId, CommandRejection> {
        self.check_drain()?;
        let (id, effect) = self.model.apply(expected_state, expected_revision)?;
        self.drive(Some(effect))?;
        Ok(id)
    }
    pub fn restart(&mut self, expected_state: StateIdentity) -> Result<ApplyId, CommandRejection> {
        self.check_drain()?;
        let (id, effect) = self.model.restart(expected_state)?;
        self.drive(Some(effect))?;
        Ok(id)
    }
    pub fn reconnect(
        &mut self,
        expected_state: StateIdentity,
    ) -> Result<ReconnectAdmission, CommandRejection> {
        if self.validator_shutdown {
            return Err(CommandRejection::ShuttingDown);
        }
        let (admission, effect) = self.model.reconnect(expected_state)?;
        self.drive(effect)?;
        Ok(admission)
    }
    pub fn choose_recovery(
        &mut self,
        expected_state: StateIdentity,
        token: SelectionToken,
    ) -> Result<(), CommandRejection> {
        self.check_drain()?;
        let effect = self.model.choose_recovery(expected_state, token)?;
        self.drive(effect)
    }
    /// Decide the toggle from confirmed product state, never from queued media work.
    pub fn toggle_pause(&mut self, attempt: AttemptId) -> Result<SubmitStatus, CommandRejection> {
        if self
            .model
            .active()
            .is_some_and(|active| active.playback() == PlaybackState::Paused)
        {
            self.resume(self.model.state_identity(), attempt)?;
            return Ok(SubmitStatus::Accepted);
        }
        self.pause(attempt)
    }
    /// Explicit pause never resumes a Paused incumbent. Share the same typed
    /// guards and prepare → admit → observed transaction with the toggle route.
    pub(crate) fn pause(&mut self, attempt: AttemptId) -> Result<SubmitStatus, CommandRejection> {
        self.check_playback()?;
        let request = self.model.prepare_pause(attempt)?;
        let status = self.runner.submit_immediate(
            attempt,
            ImmediateIntent::SetPaused {
                request,
                paused: true,
            },
        );
        if status == SubmitStatus::Accepted {
            self.model.pause_admitted(attempt, request);
        }
        Ok(status)
    }
    pub fn resume(
        &mut self,
        expected_state: StateIdentity,
        attempt: AttemptId,
    ) -> Result<ApplyId, CommandRejection> {
        self.check_playback()?;
        let (id, effect) = self.model.resume(expected_state, attempt)?;
        self.drive(Some(effect))?;
        Ok(id)
    }
    fn check_playback(&self) -> Result<(), CommandRejection> {
        self.check_drain()
    }
    pub fn close(&mut self, expected_state: StateIdentity) -> Result<(), CommandRejection> {
        let effect = self.model.close(expected_state)?;
        self.validator.clear_watch();
        self.installed_watch = None;
        self.cancel_validation();
        self.drive(effect)?;
        self.finish_drain();
        Ok(())
    }
    pub fn quit(&mut self) {
        let effect = self.model.quit();
        self.validator.clear_watch();
        self.installed_watch = None;
        self.cancel_validation();
        if !self.validator_shutdown {
            self.validator_shutdown = true;
            self.validator.shutdown();
        }
        let _ = self.drive(effect);
        self.finish_drain();
    }
    fn cancel_validation(&mut self) {
        self.prepared = None;
        self.pending_validation = None;
        self.deferred_ready = None;
        if let Some(request) = &self.validation {
            self.validator.cancel_validation(request.key);
        }
    }
    fn protocol_failure(
        settings: DraftSettings,
        operation: &str,
        diagnostic: &str,
    ) -> ApplyFailure {
        ApplyFailure::new(
            FailureCategory::Lifecycle(LifecycleFailure::Protocol),
            Stage::Unknown,
            Cause::Generic,
            settings,
            operation,
            diagnostic,
        )
    }
    fn submit_pending_validation(&mut self) -> Result<(), CommandRejection> {
        if self.validation.is_some() || self.validator_shutdown {
            return Ok(());
        }
        let Some(request) = self.pending_validation.as_ref() else {
            return Ok(());
        };
        if self.model.validation_request() != Some(request) {
            self.pending_validation = None;
            return Ok(());
        }
        let Some(target) = self.model.watch_target().cloned() else {
            return Err(CommandRejection::Disconnected);
        };
        if self.installed_watch.as_ref() != Some(&target) {
            if let Err(error) = self.validator.watch(target.clone()) {
                let request = self
                    .pending_validation
                    .take()
                    .ok_or(CommandRejection::Disconnected)?;
                let failure = ApplyFailure::new(
                    FailureCategory::Validation(ValidationLayer::Discovery),
                    Stage::Prevalidation,
                    Cause::Generic,
                    request.settings.clone(),
                    "watch_capture",
                    error.to_string(),
                );
                self.model.validation_failed(request, failure);
                return Err(match error {
                    SubmitFailure::CapacityUnavailable => CommandRejection::CapacityUnavailable,
                    SubmitFailure::Disconnected => CommandRejection::Disconnected,
                });
            }
            self.installed_watch = Some(target);
        }
        self.poll_observation();
        let Some(stamp) = self
            .model
            .observation()
            .map(|observation| observation.stamp)
        else {
            // Worker creation is not subscription or initial-scan readiness.
            return Ok(());
        };
        let Some(request) = self.model.refresh_validation_stamp(stamp) else {
            self.pending_validation = None;
            return Ok(());
        };
        self.pending_validation = None;
        if let Some(RecoveryObservation {
            video: crate::domain::capture::VideoPresence::Unknown(failure),
            ..
        }) = self.model.observation()
        {
            let failure = failure.clone();
            self.model.validation_failed(request, failure);
            return Ok(());
        }
        match self.validator.begin_validate(request.clone()) {
            Ok(()) => self.validation = Some(request),
            Err(error) => {
                let failure = ApplyFailure::new(
                    FailureCategory::Validation(ValidationLayer::Discovery),
                    Stage::Prevalidation,
                    Cause::Generic,
                    request.settings.clone(),
                    "submit_validation",
                    error.to_string(),
                );
                self.model.validation_failed(request, failure);
                return Err(match error {
                    SubmitFailure::CapacityUnavailable => CommandRejection::CapacityUnavailable,
                    SubmitFailure::Disconnected => CommandRejection::Disconnected,
                });
            }
        }
        Ok(())
    }
    fn poll_observation(&mut self) {
        let Some(observation) = self.validator.poll_recovery() else {
            return;
        };
        let effect = self.model.recovery_observed(&observation);
        if self
            .validation
            .as_ref()
            .is_some_and(|request| self.model.validation_request() != Some(request))
        {
            if let Some(request) = &self.validation {
                self.validator.cancel_validation(request.key);
            }
            self.prepared = None;
        }
        let _ = self.drive(effect);
    }
    fn accept_audio_status(&mut self, attempt: AttemptId, status: AudioAvailability) {
        let Some(lease) = self.lease.as_mut().filter(|lease| {
            lease.key.attempt == attempt && !lease.stopping && !lease.owner_stopped
        }) else {
            return;
        };
        let epoch = match &status {
            AudioAvailability::Opening { epoch }
            | AudioAvailability::Detaching { epoch }
            | AudioAvailability::Blocked { epoch, .. }
            | AudioAvailability::Switching { epoch, .. } => Some(*epoch),
            AudioAvailability::Active { route, .. } => Some(route.epoch()),
            AudioAvailability::Disabled | AudioAvailability::Silent { .. } => None,
        };
        if let Some(epoch) = epoch
            && lease.audio_epoch != Some(epoch)
        {
            // The only owner-created transaction is the initial epoch.
            // Every later epoch must have been admitted by this coordinator.
            if lease.last_audio_epoch != 0 || epoch.get() != 1 || !lease.audio_retired {
                return;
            }
            lease.audio_epoch = Some(epoch);
            lease.last_audio_epoch = epoch.get();
            lease.audio_stamp = Some(lease.watch);
            lease.audio_retired = false;
            lease.audio_attempted = Some(lease.watch);
        }
        if lease.audio_detaching
            && matches!(
                status,
                AudioAvailability::Active { .. } | AudioAvailability::Opening { .. }
            )
        {
            return;
        }
        match &status {
            AudioAvailability::Disabled if lease.settings.audio.enabled() => return,
            AudioAvailability::Switching { revision, .. }
                if *revision != self.output.revision() =>
            {
                return;
            }
            AudioAvailability::Active { route }
                if !matches!(&lease.settings.audio, AudioSelection::Enabled { source: desired } if desired == route.source())
                    || lease.audio_epoch != Some(route.epoch())
                    || lease.audio_stamp != Some(route.watch())
                    || route.attempt() != attempt
                    || route.generation().get() != attempt.get()
                    || route.destination() != self.output.target()
                    || route.output_revision() != self.output.revision()
                    || self.model.observation().is_some_and(|observation| {
                        matches!(
                            observation.audio,
                            SourcePresence::Absent(_) | SourcePresence::Unknown(_)
                        )
                    }) =>
            {
                return;
            }
            _ => {}
        }
        if matches!(
            &status,
            AudioAvailability::Silent {
                reason: AudioSilence::Failed(_)
            }
        ) {
            lease.audio_terminal = true;
        }
        if matches!(
            status,
            AudioAvailability::Detaching { .. } | AudioAvailability::Blocked { .. }
        ) {
            lease.audio_detaching = true;
        }
        self.audio = Some(status);
    }
    fn reconcile_audio(&mut self) {
        let active = self.model.active();
        let opening = self.model.opening().map(|(key, _)| key);
        let Some((stamp, audio)) = self
            .model
            .observation()
            .map(|observation| (observation.stamp, observation.audio.clone()))
        else {
            return;
        };
        let Some(lease) = self.lease.as_mut().filter(|lease| {
            !lease.stopping
                && !lease.blocked
                && !lease.owner_stopped
                && (active.is_some_and(|active| active.attempt() == lease.key.attempt)
                    || opening == Some(lease.key))
        }) else {
            return;
        };
        let attempt = lease.key.attempt;
        let paused = active.map_or(lease.playback == InitialPlayback::Paused, |active| {
            active.playback() != PlaybackState::Live
        });
        let AudioSelection::Enabled { source } = &lease.settings.audio else {
            return;
        };
        if lease.audio_terminal {
            return;
        }
        match audio {
            SourcePresence::Absent(error) | SourcePresence::Unknown(error) => {
                if let Some(epoch) = lease.audio_epoch {
                    if !lease.audio_detaching
                        && self
                            .runner
                            .submit_immediate(attempt, ImmediateIntent::DetachAudio { epoch })
                            == SubmitStatus::Accepted
                    {
                        lease.audio_detaching = true;
                        self.audio = Some(AudioAvailability::Detaching { epoch });
                    }
                } else if lease.audio_retired {
                    self.audio = Some(AudioAvailability::Silent {
                        reason: AudioSilence::WaitingForSource(error),
                    });
                }
            }
            SourcePresence::Present
                if lease.verified
                    && self.output.target().is_some()
                    && !lease.audio_terminal
                    && !paused
                    && lease.audio_retired
                    && lease.audio_epoch.is_none()
                    && (lease.audio_attempted != Some(stamp)
                        || matches!(
                            self.audio,
                            Some(AudioAvailability::Silent {
                                reason: AudioSilence::Output(_)
                            })
                        )) =>
            {
                let Some(value) = lease.last_audio_epoch.checked_add(1) else {
                    return;
                };
                let Some(epoch) = AudioEpoch::new(value) else {
                    return;
                };
                lease.audio_attempted = Some(stamp);
                if self.runner.submit_immediate(
                    attempt,
                    ImmediateIntent::AttachAudio {
                        epoch,
                        source: source.clone(),
                        stamp,
                    },
                ) == SubmitStatus::Accepted
                {
                    lease.audio_epoch = Some(epoch);
                    lease.last_audio_epoch = value;
                    lease.audio_stamp = Some(stamp);
                    lease.audio_retired = false;
                    self.audio = Some(AudioAvailability::Opening { epoch });
                }
            }
            _ => {}
        }
    }
    fn retire_lease_selection(&mut self) {
        if let Some(token) = self.lease.as_mut().and_then(|lease| lease.choice.take()) {
            self.validator.retire_selection(token);
        }
    }
    fn drive(&mut self, mut effect: Option<ModelEffect>) -> Result<(), CommandRejection> {
        self.retire_startup_restore_guard();
        // Each event has at most one next effect. Synchronous no-resource failure
        // can advance through the single rollback, never create an unbounded retry.
        while let Some(next) = effect.take() {
            match next {
                ModelEffect::Validate(request) => {
                    if let Some(old) = &self.validation
                        && old != &request
                    {
                        self.validator.cancel_validation(old.key);
                    }
                    self.pending_validation = Some(request);
                    self.submit_pending_validation()?;
                }
                ModelEffect::Open { key, request } => {
                    if self.lease.is_some() {
                        let failure = Self::protocol_failure(
                            request.settings,
                            "begin_open",
                            "previous physical lease still owned",
                        );
                        self.model.cleanup_blocked(key.attempt, failure);
                        continue;
                    }
                    if let Some(next) = self.model.cutover_validation(key) {
                        self.prepared = None;
                        effect = Some(next);
                        continue;
                    }
                    if self.model.opening().map(|(current, _)| current) != Some(key) {
                        self.prepared = None;
                        continue;
                    }
                    let prepared = self
                        .prepared
                        .take()
                        .and_then(|(identity, prepared)| (identity == request).then_some(prepared));
                    let Some(prepared) = prepared else {
                        let failure = Self::protocol_failure(
                            request.settings,
                            "begin_open",
                            "matching prepared value unavailable",
                        );
                        let _ = self.model.open_failed(key, failure);
                        if let Some(token) = request.choice {
                            self.validator.retire_selection(token);
                        }
                        effect = self.model.barrier_complete(key.attempt);
                        continue;
                    };
                    self.audio = None;
                    self.lease = Some(Lease {
                        key,
                        settings: request.settings,
                        owner_stopped: false,
                        stopping: false,
                        verified: false,
                        blocked: false,
                        watch: request.watch,
                        playback: request.playback,
                        audio_epoch: None,
                        last_audio_epoch: 0,
                        audio_stamp: None,
                        audio_retired: true,
                        audio_detaching: false,
                        audio_attempted: None,
                        audio_terminal: false,
                        choice: request.choice,
                    });
                    match self.runner.begin_open(
                        key,
                        prepared,
                        self.gain,
                        request.playback,
                        self.output.clone(),
                    ) {
                        Ok(()) => {}
                        Err(StartFailure::NoResourcesCreated(failure)) => {
                            let _ = self.model.open_failed(key, failure);
                            self.retire_lease_selection();
                            self.lease = None;
                            effect = self.model.barrier_complete(key.attempt);
                        }
                        Err(StartFailure::ResourcesCreated(failure)) => {
                            effect = self.model.open_failed(key, failure);
                        }
                    }
                }
                ModelEffect::Stop { attempt, reason } => {
                    let Some(lease) = self
                        .lease
                        .as_mut()
                        .filter(|lease| lease.key.attempt == attempt)
                    else {
                        let settings = self.model.last_valid().map_or_else(
                            || self.model.draft().settings.clone(),
                            |applied| applied.settings().clone(),
                        );
                        let failure = Self::protocol_failure(
                            settings,
                            "stop",
                            "owned attempt has no adapter lease",
                        );
                        self.model.cleanup_blocked(attempt, failure);
                        continue;
                    };
                    lease.stopping = true;
                    let reason = match reason {
                        StopIntent::Replace => StopReason::Replace,
                        StopIntent::Failed => StopReason::Failed,
                        StopIntent::Close => StopReason::Close,
                        StopIntent::Quit => StopReason::Quit,
                    };
                    match self.runner.stop(attempt, reason) {
                        StopSubmission::Accepted | StopSubmission::AlreadyStopping => {}
                        StopSubmission::NoResourcesCreated if !lease.verified => {
                            let choice = lease.choice.take();
                            if let Some(token) = choice {
                                self.validator.retire_selection(token);
                            }
                            self.lease = None;
                            effect = self.model.barrier_complete(attempt);
                        }
                        StopSubmission::NoResourcesCreated => {
                            let failure = Self::protocol_failure(
                                lease.settings.clone(),
                                "stop",
                                "verified session cannot report no resources created",
                            );
                            lease.blocked = true;
                            self.model.cleanup_blocked(attempt, failure);
                        }
                        StopSubmission::Blocked(failure) => {
                            lease.blocked = true;
                            self.model.cleanup_blocked(attempt, failure);
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn known_key(&self, key: AttemptKey) -> bool {
        self.lease.as_ref().is_some_and(|lease| lease.key == key)
    }
    fn known_attempt(&self, attempt: AttemptId) -> bool {
        self.lease
            .as_ref()
            .is_some_and(|lease| lease.key.attempt == attempt)
    }
    fn handle_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::OpenVerified { key, receipt } => self.commit_receipt(key, *receipt),
            SessionEvent::OpenFailed { key, failure } => {
                if !self.known_key(key) {
                    return;
                }
                let effect = self.model.open_failed(key, failure);
                let _ = self.drive(effect);
            }
            SessionEvent::SessionFailed { attempt, failure } => {
                if !self.known_attempt(attempt) {
                    return;
                }
                let effect = self.model.session_failed(attempt, failure);
                let _ = self.drive(effect);
            }
            SessionEvent::StreamEnded {
                attempt,
                reason,
                error,
            } => {
                let Some(lease) = self
                    .lease
                    .as_ref()
                    .filter(|lease| lease.key.attempt == attempt)
                else {
                    return;
                };
                let failure = ApplyFailure::new(
                    FailureCategory::Session,
                    Stage::Unknown,
                    Cause::Generic,
                    lease.settings.clone(),
                    "stream_ended",
                    format!("capture stream ended: reason={reason}, error={error}"),
                );
                let effect = self.model.video_lost(
                    attempt,
                    LossEvidence::StreamEnded { reason, error },
                    failure,
                );
                let _ = self.drive(effect);
            }
            SessionEvent::PauseObserved {
                attempt,
                request,
                paused,
            } => {
                if self.known_attempt(attempt)
                    && !self.lease.as_ref().is_some_and(|lease| lease.stopping)
                {
                    self.model.pause_observed(attempt, request, paused);
                }
            }
            SessionEvent::OwnerStopped { attempt, outcome } => {
                let Some(lease) = self
                    .lease
                    .as_ref()
                    .filter(|lease| lease.key.attempt == attempt && !lease.owner_stopped)
                else {
                    return;
                };
                if self.model.active().map(|active| active.attempt()) == Some(attempt)
                    || self
                        .model
                        .opening()
                        .is_some_and(|(key, _)| key.attempt == attempt)
                {
                    let failure = ApplyFailure::new(
                        FailureCategory::Lifecycle(LifecycleFailure::Acknowledgement),
                        Stage::Unknown,
                        Cause::Generic,
                        lease.settings.clone(),
                        "owner_stopped",
                        "owner stopped before product requested cleanup",
                    );
                    // Owner already retired. Revoke live product state, without
                    // submitting another command to the destroyed owner.
                    let _ = self.model.session_failed(attempt, failure);
                }
                if let Err(failure) = outcome {
                    self.model.cleanup_error(attempt, failure);
                }
                if let Some(lease) = &mut self.lease {
                    // The matching destruction acknowledgement retires audio
                    // even while the native parent still awaits release.
                    self.audio = None;
                    lease.owner_stopped = true;
                    lease.stopping = true;
                }
            }
            SessionEvent::NativeReleased { attempt } => {
                if !self
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.key.attempt == attempt && lease.owner_stopped)
                {
                    return;
                }
                self.retire_lease_selection();
                self.lease = None;
                self.audio = None;
                let effect = self.model.barrier_complete(attempt);
                let _ = self.drive(effect);
            }
            SessionEvent::CleanupBlocked { attempt, failure } => {
                let Some(lease) = self
                    .lease
                    .as_mut()
                    .filter(|lease| lease.key.attempt == attempt)
                else {
                    return;
                };
                if lease.blocked {
                    self.model.cleanup_error(attempt, failure);
                    return;
                }
                lease.blocked = true;
                self.model.cleanup_blocked(attempt, failure);
            }
            SessionEvent::AudioAvailability { attempt, status } => {
                self.accept_audio_status(attempt, status);
            }
            SessionEvent::AudioDetached {
                attempt,
                epoch,
                outcome,
            } => {
                let Some(lease) = self.lease.as_mut().filter(|lease| {
                    lease.key.attempt == attempt && !lease.stopping && !lease.owner_stopped
                }) else {
                    return;
                };
                if lease.audio_epoch.is_none()
                    && lease.audio_retired
                    && lease.last_audio_epoch == 0
                    && epoch.get() == 1
                    && lease.settings.audio.enabled()
                {
                    // The initial Add and its real retirement can coalesce before
                    // any Opening snapshot is consumed. The genuine detached
                    // receipt itself carries that consumed epoch high-water.
                    lease.audio_epoch = Some(epoch);
                    lease.last_audio_epoch = epoch.get();
                    lease.audio_retired = false;
                    lease.audio_stamp = Some(lease.watch);
                    lease.audio_attempted = Some(lease.watch);
                }
                if lease.audio_epoch != Some(epoch) || lease.audio_retired {
                    return;
                }
                match outcome {
                    Ok(()) => {
                        lease.audio_epoch = None;
                        lease.audio_retired = true;
                        lease.audio_detaching = false;
                        lease.audio_stamp = None;
                        if matches!(self.audio, Some(AudioAvailability::Detaching { .. })) {
                            self.audio = Some(AudioAvailability::Silent {
                                reason: AudioSilence::PendingRoute,
                            });
                        }
                    }
                    Err(error) => {
                        self.audio = Some(AudioAvailability::Blocked {
                            epoch,
                            error: error.clone(),
                        });
                        let failure = ApplyFailure::new(
                            FailureCategory::Lifecycle(LifecycleFailure::Protocol),
                            Stage::Open,
                            Cause::Generic,
                            lease.settings.clone(),
                            "audio_detach",
                            error.to_string(),
                        );
                        // Reuse is unsafe only when real retirement failed.
                        let effect = self.model.session_failed(attempt, failure);
                        let _ = self.drive(effect);
                    }
                }
            }
        }
    }
    fn commit_receipt(&mut self, key: AttemptKey, mut receipt: OpenReceipt) {
        if !self.known_key(key) || self.model.opening().map(|(current, _)| current) != Some(key) {
            return;
        }
        let Some((_, settings)) = self.model.opening() else {
            return;
        };
        if let AudioOutcome::Active { route } = &receipt.audio
            && receipt.matches(settings)
            && self.lease.as_ref().is_some_and(|lease| {
                route.watch() == lease.watch
                    && route.epoch().get() == lease.last_audio_epoch
                    && route.attempt() == key.attempt
                    && route.generation().get() == key.attempt.get()
                    && route.output_revision().get() <= self.output.revision().get()
            })
        {
            match &self.audio {
                Some(AudioAvailability::Silent { reason }) => {
                    // Video proof survives an independently observed, genuinely
                    // silent audio retirement. This is never an Active receipt.
                    receipt.audio = AudioOutcome::Silent {
                        source: route.source().clone(),
                        reason: reason.clone(),
                    };
                }
                Some(
                    AudioAvailability::Detaching { .. }
                    | AudioAvailability::Opening { .. }
                    | AudioAvailability::Switching { .. }
                    | AudioAvailability::Blocked { .. },
                ) => {
                    // Retain one bounded readiness proof while the genuine
                    // audio-only barrier completes; do not destroy video.
                    self.deferred_ready = Some((key, receipt));
                    return;
                }
                Some(AudioAvailability::Active { route: observed })
                    if observed.source() == route.source()
                        && observed.output_revision() == self.output.revision()
                        && observed.destination() == self.output.target() =>
                {
                    receipt.audio = AudioOutcome::Active {
                        route: observed.clone(),
                    };
                }
                _ if route.output_revision() != self.output.revision() => {
                    self.deferred_ready = Some((key, receipt));
                    return;
                }
                _ => {}
            }
        }
        if matches!(&receipt.audio, AudioOutcome::Silent { .. })
            && let Some(AudioAvailability::Silent { reason }) = &self.audio
            && let AudioOutcome::Silent {
                reason: received, ..
            } = &mut receipt.audio
        {
            *received = reason.clone();
        }
        let readiness_matches = self.model.opening_request().is_some_and(|request| {
            matches!(
                (request.playback, receipt.readiness),
                (InitialPlayback::Live, OpenReadiness::Live)
                    | (InitialPlayback::Paused, OpenReadiness::PausedPrepared)
            )
        });
        let audio_matches = match &receipt.audio {
            AudioOutcome::Active { route } => self.lease.as_ref().is_some_and(|lease| {
                lease.audio_epoch == Some(route.epoch())
                    && lease.audio_stamp == Some(route.watch())
                    && route.watch() == lease.watch
                    && route.attempt() == key.attempt
                    && route.generation().get() == key.attempt.get()
                    && route.destination() == self.output.target()
                    && route.output_revision() == self.output.revision()
                    && matches!(&self.audio, Some(AudioAvailability::Active { route: observed })
                        if observed == route)
            }),
            AudioOutcome::Silent { .. } | AudioOutcome::Disabled => true,
        };
        let startup_audio_matches = self.strict_startup_apply != Some(key.apply)
            || key.purpose != AttemptPurpose::Candidate
            || !settings.audio.enabled()
            || matches!(&receipt.audio, AudioOutcome::Active { .. })
            || (matches!(
                &receipt.audio,
                AudioOutcome::Silent {
                    reason: AudioSilence::Output(_),
                    ..
                }
            ) && self
                .model
                .observation()
                .is_some_and(|observation| observation.audio == SourcePresence::Present));
        if !receipt.matches(settings)
            || !readiness_matches
            || !audio_matches
            || !startup_audio_matches
        {
            let failure = Self::protocol_failure(
                settings.clone(),
                "open_receipt",
                if startup_audio_matches {
                    "receipt settings or requested audio outcome mismatch"
                } else {
                    "startup restore requires active requested audio"
                },
            );
            let effect = self.model.open_failed(key, failure);
            let _ = self.drive(effect);
            return;
        }
        self.model.open_verified(key);
        if !self
            .model
            .active()
            .is_some_and(|active| active.attempt() == key.attempt)
        {
            return;
        }
        if let Some(lease) = &mut self.lease {
            lease.verified = true;
        }
        if !matches!(
            self.audio,
            Some(AudioAvailability::Detaching { .. } | AudioAvailability::Blocked { .. })
        ) {
            self.audio = Some(match receipt.audio {
                AudioOutcome::Disabled => AudioAvailability::Disabled,
                AudioOutcome::Silent { reason, .. } => AudioAvailability::Silent { reason },
                AudioOutcome::Active { route } => AudioAvailability::Active { route },
            });
        }
        // A physical choice is authorized for this one opening only, including
        // the case where its complete identity equals the saved target.
        self.retire_lease_selection();
        let watch_result = self.model.committed_watch().and_then(|target| {
            if let Some(target) = target {
                self.validator
                    .watch(target.clone())
                    .map_err(|error| match error {
                        SubmitFailure::CapacityUnavailable => CommandRejection::CapacityUnavailable,
                        SubmitFailure::Disconnected => CommandRejection::Disconnected,
                    })?;
                self.installed_watch = Some(target);
            }
            Ok(())
        });
        if let Err(error) = watch_result {
            let settings = self.lease.as_ref().map(|lease| lease.settings.clone());
            if let Some(settings) = settings {
                let failure =
                    Self::protocol_failure(settings, "committed_watch", &error.to_string());
                let effect = self.model.session_failed(key.attempt, failure);
                let _ = self.drive(effect);
            }
            return;
        }
        if let Some(active) = self
            .model
            .active()
            .filter(|active| active.attempt() == key.attempt)
        {
            self.verified_open = Some(VerifiedOpen {
                key,
                applied: active.applied().clone(),
            });
        }
    }
    pub fn poll(&mut self) {
        // Subscribe/observe health first; positive removals invalidate even a
        // coalesced Present before any cached media readiness can be committed.
        self.poll_observation();
        let mut ready = self.deferred_ready.take();
        // One bounded batch. Terminal resource/health events beat readiness even
        // if a faulty adapter presents a cached Ready before its acknowledgement.
        for _ in 0..16 {
            let Some(event) = self.runner.poll() else {
                break;
            };
            match event {
                SessionEvent::OpenVerified { key, receipt } if self.known_key(key) => {
                    if self.model.opening().map(|(current, _)| current) == Some(key) {
                        ready = Some((key, *receipt));
                    }
                }
                other => self.handle_event(other),
            }
        }
        self.poll_observation();
        self.reconcile_audio();
        if let Some((key, receipt)) = ready {
            self.commit_receipt(key, receipt);
        }
        if let Some(result) = self.validator.poll_validation()
            && self
                .validation
                .as_ref()
                .is_some_and(|request| request.key == result.request.key)
        {
            let Some(request) = self.validation.take() else {
                return;
            };
            if self.model.validation_request() == Some(&request) {
                if result.request != request
                    || result.stamp.watch != request.watch.watch
                    || result.stamp.epoch.get() < request.watch.epoch.get()
                {
                    let failure = Self::protocol_failure(
                        request.settings.clone(),
                        "validation_result",
                        "terminal validation identity or fresh stamp mismatch",
                    );
                    if let Some(token) = request.choice {
                        self.validator.retire_selection(token);
                    }
                    self.model.validation_failed(request, failure);
                } else if let Some(stamp) = self
                    .model
                    .observation()
                    .map(|observation| observation.stamp)
                    && stamp.watch == result.stamp.watch
                    && stamp.epoch.get() > result.stamp.epoch.get()
                {
                    // A fresher authoritative observation supersedes this completed
                    // snapshot without turning invalidation into a real open failure.
                    if let Some(refreshed) = self.model.refresh_validation_stamp(stamp) {
                        let _ = self.drive(Some(ModelEffect::Validate(refreshed)));
                    }
                } else {
                    match result.result {
                        ValidationOutcome::Prepared(prepared) => {
                            let settings = V::prepared_settings(&prepared).clone();
                            let stamp = V::prepared_stamp(&prepared);
                            if stamp != result.stamp {
                                let failure = Self::protocol_failure(
                                    request.settings.clone(),
                                    "validation_result",
                                    "prepared evidence and completed observation disagree",
                                );
                                if let Some(token) = request.choice {
                                    self.validator.retire_selection(token);
                                }
                                self.model.validation_failed(request, failure);
                            } else if let Some(accepted) =
                                self.model.accept_prepared(&request, settings, stamp)
                            {
                                self.prepared = Some((accepted.clone(), prepared));
                                let effect = self.model.validation_succeeded(&accepted);
                                let _ = self.drive(effect);
                            } else {
                                let failure = Self::protocol_failure(
                                    request.settings.clone(),
                                    "validation_result",
                                    "prepared settings do not preserve the requested tuple and audio",
                                );
                                if let Some(token) = request.choice {
                                    self.validator.retire_selection(token);
                                }
                                self.model.validation_failed(request, failure);
                            }
                        }
                        ValidationOutcome::SelectionRequired(candidates) => {
                            self.model.selection_required(&request, candidates);
                            if let Some(token) = request.choice {
                                self.validator.retire_selection(token);
                            }
                        }
                        ValidationOutcome::Failed(failure) => {
                            // Failed results also carry a genuinely completed
                            // scan stamp; latch that reality, not the old request.
                            let completed = self
                                .model
                                .refresh_validation_stamp(result.stamp)
                                .unwrap_or(request);
                            if let Some(token) = completed.choice {
                                self.validator.retire_selection(token);
                            }
                            self.model.validation_failed(completed, failure);
                        }
                    }
                }
            }
        }
        let _ = self.submit_pending_validation();
        if self.validation.is_none() && self.pending_validation.is_none() {
            let effect = self.model.continue_recovery();
            let _ = self.drive(effect);
        }
        self.reconcile_audio();
        self.finish_drain();
    }
    fn finish_drain(&mut self) {
        self.retire_startup_restore_guard();
        let retired = self.validator_shutdown && self.validator.shutdown_complete();
        self.model.drain_complete(
            self.validation.is_none() && self.pending_validation.is_none(),
            retired,
        );
    }
    /// Retain an admitted application preference, including owner-free replacement
    /// gaps. A healthy Opening/Ready owner must admit it before it is retained.
    pub fn set_gain(&mut self, gain: PlaybackGain) -> SubmitStatus {
        if !self.gain_admission_open() {
            return SubmitStatus::Closing;
        }
        if let Some(lease) = &self.lease
            && !lease.stopping
            && !lease.owner_stopped
        {
            let status = self
                .runner
                .submit_immediate(lease.key.attempt, ImmediateIntent::SetGain(gain));
            if status != SubmitStatus::Accepted {
                return status;
            }
        }
        self.gain = gain;
        SubmitStatus::Accepted
    }
    /// Shared known Closing gates for preference admission and presentation.
    /// A healthy owner's port may still reject a submission independently.
    pub(crate) fn gain_admission_open(&self) -> bool {
        self.check_drain().is_ok()
            && !matches!(
                self.model.phase(),
                ProductPhase::Stopping | ProductPhase::ShutdownReady
            )
            && !matches!(self.model.cleanup(), CleanupStatus::Blocked { .. })
            && !self.lease.as_ref().is_some_and(|lease| lease.blocked)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::app::ports::{AudioOutcome, FactStatus, ValidationResult, VerificationSummary};
    use crate::domain::{
        capture::{
            AudioSelection, CaptureMode, CapturedFourCc, DeviceIdentity, FrameRate, FrameSize,
            UsbTopology,
        },
        failure::{Cause, FailureCategory, Stage, ValidationLayer},
        state::{
            AttemptPurpose, CleanupStatus, PauseRequestId, PlaybackState, ProductPhase,
            ValidationKey,
        },
    };

    fn output_fixture() -> crate::domain::output::LiveSinkTarget {
        crate::domain::output::LiveSinkTarget::new(
            crate::domain::output::SinkIdentity::new("fixture.output".into(), vec![]).unwrap(),
            std::num::NonZeroU64::new(20).unwrap(),
            10,
        )
        .unwrap()
    }
    fn route_fixture(
        attempt: AttemptId,
        epoch: AudioEpoch,
        stamp: WatchStamp,
        source: crate::domain::capture::AudioSourceIdentity,
    ) -> crate::media::loopback::LoopbackReceipt {
        crate::media::loopback::LoopbackReceipt::for_test(
            crate::media::controller::Generation::new(attempt.get()).unwrap(),
            attempt,
            epoch,
            stamp,
            source,
            output_fixture(),
            OutputRevision::first(),
        )
    }
    fn settings(rate: u32) -> DraftSettings {
        DraftSettings {
            video: crate::domain::capture::ModeRequest {
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
    fn failure(
        settings: DraftSettings,
        category: FailureCategory,
        diagnostic: &str,
    ) -> ApplyFailure {
        ApplyFailure::new(
            category,
            Stage::Unknown,
            Cause::Generic,
            settings,
            "test",
            diagnostic,
        )
    }
    fn receipt(settings: DraftSettings) -> OpenReceipt {
        OpenReceipt {
            settings,
            verification: VerificationSummary {
                captured_fourcc: FactStatus::Unverified,
                decoded_size: FactStatus::ObservedCompatible,
                nominal_rate: FactStatus::Approximate,
            },
            audio: AudioOutcome::Disabled,
            readiness: OpenReadiness::Live,
        }
    }
    #[derive(Clone)]
    struct TestPrepared {
        settings: DraftSettings,
        stamp: WatchStamp,
    }
    #[derive(Default)]
    struct Validator {
        request: Option<ValidationRequest>,
        result: Option<ValidationResult<TestPrepared>>,
        requests: Vec<ValidationRequest>,
        cancelled: bool,
        shutdown: bool,
        retired: bool,
        reject: Option<SubmitFailure>,
        reject_watch: Option<SubmitFailure>,
        target: Option<RecoveryWatchTarget>,
        observation: Option<RecoveryObservation>,
        watches: Vec<RecoveryWatchTarget>,
        observed: Option<WatchStamp>,
        cutovers: Vec<ValidationRequest>,
        delay_initial: bool,
        defer_cutover: bool,
        watcher_join_pending: bool,
        grant: Option<SelectionToken>,
        retired_choices: Vec<SelectionToken>,
    }
    impl Validator {
        fn finish(&mut self, result: Result<(), ApplyFailure>) {
            let request = self.request.take().unwrap();
            let stamp = self.observed.unwrap_or(request.watch);
            self.result = Some(ValidationResult {
                stamp,
                result: match result {
                    Ok(()) => ValidationOutcome::Prepared(TestPrepared {
                        settings: request.settings.clone(),
                        stamp,
                    }),
                    Err(failure) => ValidationOutcome::Failed(failure),
                },
                request,
            });
        }
    }
    impl DraftValidator for Validator {
        type Prepared = TestPrepared;
        fn prepared_settings(prepared: &Self::Prepared) -> &DraftSettings {
            &prepared.settings
        }
        fn prepared_stamp(prepared: &Self::Prepared) -> WatchStamp {
            prepared.stamp
        }
        fn begin_validate(&mut self, request: ValidationRequest) -> Result<(), SubmitFailure> {
            if let Some(rejection) = self.reject.take() {
                return Err(rejection);
            }
            if self.request.is_some() || self.result.is_some() {
                return Err(SubmitFailure::CapacityUnavailable);
            }
            let cutover = self
                .requests
                .iter()
                .any(|prior| prior.key == request.key && prior.watch.watch != request.watch.watch);
            if cutover {
                self.cutovers.push(request.clone());
            } else {
                self.requests.push(request.clone());
            }
            if let Some(token) = request.choice {
                self.grant = Some(token);
            }
            self.request = Some(request);
            if cutover && !self.defer_cutover {
                self.finish(Ok(()));
            }
            Ok(())
        }
        fn poll_validation(&mut self) -> Option<ValidationResult<Self::Prepared>> {
            self.result.take()
        }
        fn cancel_validation(&mut self, _: ValidationKey) {
            self.cancelled = true;
        }
        fn watch(&mut self, target: RecoveryWatchTarget) -> Result<(), SubmitFailure> {
            if let Some(rejection) = self.reject_watch.take() {
                return Err(rejection);
            }
            if !self.delay_initial {
                self.observation = Some(RecoveryObservation {
                    stamp: WatchStamp {
                        watch: target.watch,
                        epoch: crate::domain::capture::ObservationEpoch::new(1).unwrap(),
                    },
                    video: crate::domain::capture::VideoPresence::Present,
                    audio: if target.audio.enabled() {
                        SourcePresence::Present
                    } else {
                        SourcePresence::Disabled
                    },
                    last_video_removal: None,
                });
            }
            self.watches.push(target.clone());
            self.target = Some(target);
            Ok(())
        }
        fn poll_recovery(&mut self) -> Option<RecoveryObservation> {
            let observation = self.observation.take();
            if let Some(observation) = &observation
                && self.target.as_ref().map(|target| target.watch) == Some(observation.stamp.watch)
                && self.observed.is_none_or(|old| {
                    old.watch != observation.stamp.watch
                        || old.epoch.get() <= observation.stamp.epoch.get()
                })
            {
                self.observed = Some(observation.stamp);
            }
            observation
        }
        fn clear_watch(&mut self) {
            self.target = None;
            self.observation = None;
            self.grant = None;
        }
        fn retire_selection(&mut self, token: SelectionToken) {
            if self.grant == Some(token) {
                self.grant = None;
                self.retired_choices.push(token);
            }
        }
        fn shutdown(&mut self) {
            self.shutdown = true;
        }
        fn shutdown_complete(&mut self) -> bool {
            self.retired && !self.watcher_join_pending
        }
    }
    #[derive(Default)]
    struct Runner {
        events: VecDeque<SessionEvent>,
        opens: Vec<(AttemptKey, DraftSettings, PlaybackGain)>,
        stops: Vec<(AttemptId, StopReason)>,
        immediate: Option<SubmitStatus>,
        intents: Vec<(AttemptId, ImmediateIntent)>,
        start_failure: Option<StartFailure>,
        stop_failure: Option<StopSubmission>,
        playback: Vec<InitialPlayback>,
        outputs: Vec<OutputPlan>,
    }
    impl Runner {
        fn verified(&mut self) {
            let (key, settings, _) = self.opens.last().unwrap();
            let mut receipt = receipt(settings.clone());
            receipt.readiness = match self.playback.last().unwrap() {
                InitialPlayback::Live => OpenReadiness::Live,
                InitialPlayback::Paused => OpenReadiness::PausedPrepared,
            };
            self.events.push_back(SessionEvent::OpenVerified {
                key: *key,
                receipt: Box::new(receipt),
            });
        }
        fn fail(&mut self, detail: &str) {
            let (key, settings, _) = self.opens.last().unwrap();
            self.events.push_back(SessionEvent::OpenFailed {
                key: *key,
                failure: failure(settings.clone(), FailureCategory::Session, detail),
            });
        }
        fn barrier(&mut self, attempt: AttemptId) {
            self.events.push_back(SessionEvent::OwnerStopped {
                attempt,
                outcome: Ok(()),
            });
            self.events
                .push_back(SessionEvent::NativeReleased { attempt });
        }
    }
    impl SessionRunner for Runner {
        type Prepared = TestPrepared;
        fn begin_open(
            &mut self,
            key: AttemptKey,
            prepared: TestPrepared,
            gain: PlaybackGain,
            playback: InitialPlayback,
            output: OutputPlan,
        ) -> Result<(), StartFailure> {
            self.opens.push((key, prepared.settings, gain));
            self.playback.push(playback);
            self.outputs.push(output);
            self.start_failure.take().map_or(Ok(()), Err)
        }
        fn stop(&mut self, attempt: AttemptId, reason: StopReason) -> StopSubmission {
            self.stops.push((attempt, reason));
            self.stop_failure.take().unwrap_or(StopSubmission::Accepted)
        }
        fn poll(&mut self) -> Option<SessionEvent> {
            self.events.pop_front()
        }
        fn submit_immediate(
            &mut self,
            attempt: AttemptId,
            intent: ImmediateIntent,
        ) -> SubmitStatus {
            self.intents.push((attempt, intent));
            self.immediate.take().unwrap_or(SubmitStatus::Accepted)
        }
    }
    type Engine = ApplyCoordinator<Validator, Runner>;
    fn engine() -> Engine {
        ApplyCoordinator::new(
            settings(60),
            PlaybackGain::default(),
            Validator::default(),
            Runner::default(),
        )
    }
    fn apply(engine: &mut Engine) -> ApplyId {
        if !engine.output_initialized {
            engine
                .set_output_plan(OutputPlan::Target {
                    revision: OutputRevision::first(),
                    target: output_fixture(),
                })
                .unwrap();
        }
        engine
            .apply(
                engine.model().state_identity(),
                engine.model().draft().revision,
            )
            .unwrap()
    }
    fn validate(engine: &mut Engine) {
        engine.validator_mut().finish(Ok(()));
        engine.poll();
    }
    fn initial(engine: &mut Engine) -> AttemptId {
        apply(engine);
        validate(engine);
        engine.runner_mut().verified();
        engine.poll();
        engine.model().active().unwrap().attempt()
    }
    fn switch(engine: &mut Engine) -> (AttemptId, AttemptId) {
        let old = initial(engine);
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        apply(engine);
        validate(engine);
        engine.runner_mut().barrier(old);
        engine.poll();
        (old, engine.runner_mut().opens.last().unwrap().0.attempt)
    }

    fn confirm_pause(engine: &mut Engine, attempt: AttemptId) {
        assert_eq!(
            engine.toggle_pause(attempt).unwrap(),
            SubmitStatus::Accepted
        );
        let PlaybackState::PausePending { request } = engine.model().active().unwrap().playback()
        else {
            panic!("pause admission must remain pending");
        };
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::PauseObserved {
                attempt,
                request,
                paused: true,
            });
        engine.poll();
        assert_eq!(
            engine.model().active().unwrap().playback(),
            PlaybackState::Paused
        );
    }

    #[test]
    fn verified_open_uses_exact_committed_settings_and_is_consumed_once() {
        let mut engine = engine();
        let apply = apply(&mut engine);
        assert!(engine.take_verified_open().is_none());
        validate(&mut engine);
        assert!(engine.take_verified_open().is_none());
        let key = engine.runner.opens.last().unwrap().0;
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        // Both coalesced and later duplicate receipts must produce one event.
        engine.runner.verified();
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.installed_watch.as_ref(), engine.model.watch_target());
        let event = engine.take_verified_open().unwrap();
        assert_eq!(event.key, key);
        assert_eq!(event.key.apply, apply);
        assert_eq!(event.applied.settings(), &settings(60));
        assert_eq!(engine.model.draft().settings, settings(30));
        assert!(engine.take_verified_open().is_none());
        engine.runner.verified();
        engine.poll();
        assert!(engine.take_verified_open().is_none());
    }

    #[test]
    fn stale_or_wrong_receipts_never_publish_verified_open() {
        let mut engine = engine();
        apply(&mut engine);
        validate(&mut engine);
        let key = engine.runner.opens.last().unwrap().0;
        for stale in [
            AttemptKey {
                apply: ApplyId::new(key.apply.get() + 1).unwrap(),
                ..key
            },
            AttemptKey {
                attempt: AttemptId::new(key.attempt.get() + 1).unwrap(),
                ..key
            },
            AttemptKey {
                purpose: AttemptPurpose::Restore,
                ..key
            },
        ] {
            engine.runner.events.push_back(SessionEvent::OpenVerified {
                key: stale,
                receipt: Box::new(receipt(settings(60))),
            });
            engine.poll();
            assert_eq!(engine.model.opening().unwrap().0, key);
            assert!(engine.take_verified_open().is_none());
        }
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(receipt(settings(30))),
        });
        engine.poll();
        assert!(engine.model.active().is_none());
        assert!(engine.model.last_valid().is_none());
        assert!(engine.take_verified_open().is_none());
        engine.runner.verified();
        engine.poll();
        assert!(engine.take_verified_open().is_none());
    }

    #[test]
    fn canceled_open_and_terminal_barrier_cannot_publish_cached_readiness() {
        for close in [false, true] {
            let mut engine = engine();
            apply(&mut engine);
            validate(&mut engine);
            let key = engine.runner.opens.last().unwrap().0;
            engine.runner.verified();
            if close {
                engine.close(engine.model.state_identity()).unwrap();
            } else {
                engine.runner.barrier(key.attempt);
            }
            engine.poll();
            assert!(engine.model.active().is_none());
            assert!(engine.take_verified_open().is_none());
        }
    }

    #[test]
    fn verified_open_requires_successful_committed_watch_installation() {
        for rejection in [
            None,
            Some(SubmitFailure::CapacityUnavailable),
            Some(SubmitFailure::Disconnected),
        ] {
            let mut engine = engine();
            apply(&mut engine);
            engine.validator.finish(Ok(()));
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
            let ValidationOutcome::Prepared(prepared) =
                &mut engine.validator.result.as_mut().unwrap().result
            else {
                panic!("prepared validation required");
            };
            prepared.settings = relocated.clone();
            engine.poll();
            let key = engine.runner.opens.last().unwrap().0;
            let prior_watch = engine.installed_watch.clone();
            engine.validator.reject_watch = rejection;
            engine.runner.verified();
            engine.poll();
            assert_eq!(engine.model.last_valid().unwrap().settings(), &relocated);
            if rejection.is_some() {
                assert!(engine.model.active().is_none());
                assert_eq!(engine.installed_watch, prior_watch);
                assert!(engine.take_verified_open().is_none());
                assert_eq!(
                    engine
                        .model
                        .failures()
                        .unwrap()
                        .incumbent
                        .as_ref()
                        .unwrap()
                        .operation,
                    "committed_watch"
                );
            } else {
                assert_ne!(engine.installed_watch, prior_watch);
                assert_eq!(engine.installed_watch.as_ref(), engine.model.watch_target());
                let event = engine.take_verified_open().unwrap();
                assert_eq!(event.key, key);
                assert_eq!(event.applied.settings(), &relocated);
                assert!(engine.take_verified_open().is_none());
            }
        }
    }

    #[test]
    fn strict_startup_restore_refuses_silent_enabled_audio_without_changing_user_apply() {
        use crate::domain::capture::{AudioError, AudioSourceIdentity, VideoPresence};
        for strict in [false, true] {
            let source = AudioSourceIdentity::new("exact.capture".into(), vec![]).unwrap();
            let mut desired = settings(60);
            desired.audio = AudioSelection::Enabled {
                source: source.clone(),
            };
            let mut engine = ApplyCoordinator::new(
                desired.clone(),
                PlaybackGain::default(),
                Validator::default(),
                Runner::default(),
            );
            let apply = apply(&mut engine);
            if strict {
                engine.require_startup_restore_audio(apply).unwrap();
            }
            let absent = observation(
                &engine,
                2,
                VideoPresence::Present,
                SourcePresence::Absent(AudioError::Cancelled),
                None,
            );
            observe(&mut engine, absent);
            validate(&mut engine);
            let key = engine.runner.opens.last().unwrap().0;
            let mut opened = receipt(desired.clone());
            opened.audio = AudioOutcome::Silent {
                source,
                reason: AudioSilence::WaitingForSource(AudioError::Cancelled),
            };
            engine.runner.events.push_back(SessionEvent::OpenVerified {
                key,
                receipt: Box::new(opened),
            });
            engine.poll();
            assert_eq!(engine.model.active().is_some(), !strict);
            assert_eq!(engine.model.last_valid().is_some(), !strict);
            assert_eq!(engine.take_verified_open().is_some(), !strict);
            assert!(engine.strict_startup_apply.is_none());
            if strict {
                assert_eq!(engine.runner.stops, vec![(key.attempt, StopReason::Failed)]);
                engine.runner.barrier(key.attempt);
                engine.poll();
                assert_eq!(engine.model.phase(), ProductPhase::ErrorWithoutActive);
                assert_eq!(engine.runner.opens.len(), 1);
                assert_eq!(engine.model.draft().settings, desired);
            }
        }
    }

    #[test]
    fn strict_startup_active_audio_commits_once_and_later_loss_keeps_video() {
        use crate::domain::capture::{AudioError, AudioSourceIdentity, VideoPresence};
        let source = AudioSourceIdentity::new("exact.capture".into(), vec![]).unwrap();
        let mut desired = settings(60);
        desired.audio = AudioSelection::Enabled {
            source: source.clone(),
        };
        let mut engine = ApplyCoordinator::new(
            desired.clone(),
            PlaybackGain::default(),
            Validator::default(),
            Runner::default(),
        );
        let apply = apply(&mut engine);
        engine.require_startup_restore_audio(apply).unwrap();
        validate(&mut engine);
        let key = engine.runner.opens.last().unwrap().0;
        let route = route_fixture(
            key.attempt,
            AudioEpoch::new(1).unwrap(),
            engine.lease.as_ref().unwrap().watch,
            source.clone(),
        );
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt: key.attempt,
                status: AudioAvailability::Active {
                    route: route.clone(),
                },
            });
        let mut opened = receipt(desired.clone());
        opened.audio = AudioOutcome::Active { route };
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(opened),
        });
        engine.poll();
        let event = engine.take_verified_open().unwrap();
        assert_eq!(event.key, key);
        assert_eq!(event.applied.settings(), &desired);
        assert!(engine.strict_startup_apply.is_none());
        let absent = observation(
            &engine,
            2,
            VideoPresence::Present,
            SourcePresence::Absent(AudioError::Cancelled),
            None,
        );
        observe(&mut engine, absent);
        engine.runner.events.push_back(SessionEvent::AudioDetached {
            attempt: key.attempt,
            epoch: AudioEpoch::new(1).unwrap(),
            outcome: Ok(()),
        });
        engine.poll();
        assert_eq!(engine.model.active().unwrap().attempt(), key.attempt);
        assert!(matches!(
            engine.audio_availability(),
            Some(AudioAvailability::Silent { .. })
        ));
        assert!(engine.runner.stops.is_empty());
        assert!(engine.take_verified_open().is_none());
    }

    #[test]
    fn strict_startup_guard_rejects_stale_ids_and_retires_on_failure_or_cancel() {
        for canceled in [false, true] {
            let mut engine = engine();
            let id = apply(&mut engine);
            assert_eq!(
                engine.require_startup_restore_audio(ApplyId::new(id.get() + 1).unwrap()),
                Err(CommandRejection::StaleState)
            );
            engine.require_startup_restore_audio(id).unwrap();
            assert_eq!(engine.strict_startup_apply, Some(id));
            if canceled {
                engine.close(engine.model.state_identity()).unwrap();
                assert!(engine.strict_startup_apply.is_none());
                validate(&mut engine);
            } else {
                engine.validator.finish(Err(failure(
                    settings(60),
                    FailureCategory::Validation(ValidationLayer::Mode),
                    "startup validation refused",
                )));
                engine.poll();
            }
            assert!(engine.strict_startup_apply.is_none());
            assert!(engine.take_verified_open().is_none());
            let next = apply(&mut engine);
            assert_ne!(next, id);
            validate(&mut engine);
            engine.runner.verified();
            engine.poll();
            assert_eq!(engine.take_verified_open().unwrap().key.apply, next);
            assert_eq!(
                engine.require_startup_restore_audio(next),
                Err(CommandRejection::StaleState)
            );
        }
    }

    #[test]
    fn toggle_resume_opens_applied_not_draft_once_with_gain_from_retirement_and_opening() {
        let mut engine = engine();
        let old = initial(&mut engine);
        confirm_pause(&mut engine, old);
        let revision = engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        assert_eq!(engine.toggle_pause(old).unwrap(), SubmitStatus::Accepted);
        assert_eq!(engine.model().phase(), ProductPhase::ValidatingResume);
        assert_eq!(
            engine.validator.requests.last().unwrap().settings,
            settings(60)
        );
        assert_eq!(
            engine.validator.requests.last().unwrap().key.purpose,
            AttemptPurpose::Resume
        );
        assert!(engine.runner_mut().stops.is_empty());
        validate(&mut engine);
        assert_eq!(engine.model().phase(), ProductPhase::ClosingResume);
        let intents = engine.runner_mut().intents.len();
        let gap_gain = PlaybackGain::new(80, true).unwrap();
        assert_eq!(engine.set_gain(gap_gain), SubmitStatus::Accepted);
        assert_eq!(
            engine.runner_mut().intents.len(),
            intents,
            "retiring handle receives no gain"
        );
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::NativeReleased { attempt: old });
        engine.poll();
        assert_eq!(
            engine.runner_mut().opens.len(),
            1,
            "release without owner ack is not a barrier"
        );
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::OwnerStopped {
                attempt: old,
                outcome: Ok(()),
            });
        engine.poll();
        assert_eq!(engine.set_gain(gap_gain), SubmitStatus::Accepted);
        assert_eq!(engine.runner_mut().opens.len(), 1);
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::NativeReleased { attempt: old });
        engine.poll();
        let (new, applied, initial_gain) = engine.runner_mut().opens.last().unwrap().clone();
        assert_eq!(new.purpose, AttemptPurpose::Resume);
        assert_eq!(applied, settings(60));
        assert_eq!(initial_gain, gap_gain);
        assert_ne!(new.attempt, old);
        let opening_gain = PlaybackGain::new(81, false).unwrap();
        assert_eq!(engine.set_gain(opening_gain), SubmitStatus::Accepted);
        assert_eq!(
            engine.runner_mut().intents.last(),
            Some(&(new.attempt, ImmediateIntent::SetGain(opening_gain)))
        );
        engine.runner_mut().verified();
        engine.poll();
        assert_eq!(
            engine.model().active().unwrap().playback(),
            PlaybackState::Live
        );
        assert_eq!(engine.model().draft().revision, revision);
        assert_eq!(engine.model().draft().settings, settings(30));
        assert_eq!(engine.gain(), opening_gain);
        assert_eq!(engine.runner_mut().opens.len(), 2);
    }

    #[test]
    fn pending_pause_and_each_resume_transition_reject_overlap_without_accumulating_commands() {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine.toggle_pause(old).unwrap();
        let pending = engine.model().state_identity();
        assert_eq!(
            engine.apply(pending, engine.model().draft().revision),
            Err(CommandRejection::PlaybackBusy)
        );
        assert_eq!(engine.restart(pending), Err(CommandRejection::PlaybackBusy));
        assert_eq!(
            engine.resume(pending, old),
            Err(CommandRejection::PlaybackBusy)
        );
        let PlaybackState::PausePending { request } = engine.model().active().unwrap().playback()
        else {
            panic!("pending")
        };
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::PauseObserved {
                attempt: old,
                request,
                paused: true,
            });
        engine.poll();
        engine.resume(engine.model().state_identity(), old).unwrap();
        for phase in [
            ProductPhase::ValidatingResume,
            ProductPhase::ClosingResume,
            ProductPhase::OpeningResume,
        ] {
            assert_eq!(engine.model().phase(), phase);
            let expected = engine.model().state_identity();
            assert_eq!(
                engine.toggle_pause(old),
                Err(CommandRejection::ApplyInProgress)
            );
            assert_eq!(
                engine.resume(expected, old),
                Err(CommandRejection::ApplyInProgress)
            );
            assert_eq!(
                engine.apply(expected, engine.model().draft().revision),
                Err(CommandRejection::ApplyInProgress)
            );
            assert_eq!(
                engine.restart(expected),
                Err(CommandRejection::ApplyInProgress)
            );
            if phase == ProductPhase::ValidatingResume {
                validate(&mut engine);
            } else if phase == ProductPhase::ClosingResume {
                engine.runner_mut().barrier(old);
                engine.poll();
            }
        }
        assert_eq!(engine.runner_mut().intents.len(), 1, "no queued overlaps");
        assert_eq!(engine.runner_mut().opens.len(), 2);
    }

    #[test]
    fn resume_validation_rejection_keeps_paused_owner_and_submission_failure_is_typed() {
        for admission in [false, true] {
            let mut engine = engine();
            let old = initial(&mut engine);
            confirm_pause(&mut engine, old);
            if admission {
                engine.validator_mut().reject = Some(SubmitFailure::Disconnected);
                assert_eq!(
                    engine.resume(engine.model().state_identity(), old),
                    Err(CommandRejection::Disconnected)
                );
            } else {
                engine.resume(engine.model().state_identity(), old).unwrap();
                engine.validator_mut().finish(Err(failure(
                    settings(60),
                    FailureCategory::Validation(ValidationLayer::Mode),
                    "invalid resume",
                )));
                engine.poll();
            }
            assert_eq!(engine.model().active().unwrap().attempt(), old);
            assert_eq!(
                engine.model().active().unwrap().playback(),
                PlaybackState::Paused
            );
            assert!(engine.model().validation_rejection().is_some());
            assert!(engine.runner_mut().stops.is_empty());
            assert_eq!(engine.runner_mut().opens.len(), 1);
        }
    }

    #[test]
    fn incumbent_terminal_event_beats_resume_validation_result_and_never_resurrects_pause() {
        for valid in [false, true] {
            let mut engine = engine();
            let old = initial(&mut engine);
            confirm_pause(&mut engine, old);
            engine.resume(engine.model().state_identity(), old).unwrap();
            engine
                .runner_mut()
                .events
                .push_back(SessionEvent::SessionFailed {
                    attempt: old,
                    failure: failure(settings(60), FailureCategory::Session, "incumbent died"),
                });
            engine.validator_mut().finish(if valid {
                Ok(())
            } else {
                Err(failure(
                    settings(60),
                    FailureCategory::Validation(ValidationLayer::Mode),
                    "resume rejected",
                ))
            });
            engine.poll();
            assert!(engine.model().active().is_none());
            assert_eq!(engine.runner_mut().opens.len(), 1);
            assert_eq!(engine.runner_mut().stops.len(), 1);
            engine.runner_mut().barrier(old);
            engine.poll();
            assert_eq!(engine.runner_mut().opens.len(), if valid { 2 } else { 1 });
            assert!(engine.model().active().is_none());
        }
    }

    #[test]
    fn failed_resume_drains_created_resources_without_restore_and_clean_error_retains_gain() {
        for resources in [false, true] {
            let mut engine = engine();
            let old = initial(&mut engine);
            confirm_pause(&mut engine, old);
            engine.resume(engine.model().state_identity(), old).unwrap();
            validate(&mut engine);
            let failed = failure(settings(60), FailureCategory::Session, "resume open failed");
            engine.runner_mut().start_failure = Some(if resources {
                StartFailure::ResourcesCreated(failed)
            } else {
                StartFailure::NoResourcesCreated(failed)
            });
            engine.runner_mut().barrier(old);
            engine.poll();
            let resumed = engine.runner_mut().opens.last().unwrap().0.attempt;
            if resources {
                assert_eq!(engine.model().phase(), ProductPhase::CleaningFailedResume);
                assert!(!engine.model().can_reconnect());
                engine.runner_mut().barrier(resumed);
                engine.poll();
            }
            assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
            assert!(engine.model().failures().unwrap().resume.is_some());
            assert!(engine.model().failures().unwrap().restore.is_none());
            assert_eq!(engine.runner_mut().opens.len(), 2);
            assert_eq!(engine.validator.requests.len(), 2);
            let gain = PlaybackGain::new(77, true).unwrap();
            let intents = engine.runner_mut().intents.len();
            assert_eq!(engine.set_gain(gain), SubmitStatus::Accepted);
            assert_eq!(engine.gain(), gain);
            assert_eq!(engine.runner_mut().intents.len(), intents);
            engine.reconnect(engine.model().state_identity()).unwrap();
            validate(&mut engine);
            assert_eq!(engine.runner_mut().opens.last().unwrap().2, gain);
        }
    }

    #[test]
    fn close_or_quit_cancels_resume_at_validation_old_drain_and_new_open_without_late_reopen() {
        for quitting in [false, true] {
            for phase in [
                ProductPhase::ValidatingResume,
                ProductPhase::ClosingResume,
                ProductPhase::OpeningResume,
            ] {
                let mut engine = engine();
                let old = initial(&mut engine);
                confirm_pause(&mut engine, old);
                engine.resume(engine.model().state_identity(), old).unwrap();
                if phase != ProductPhase::ValidatingResume {
                    validate(&mut engine);
                }
                if phase == ProductPhase::OpeningResume {
                    engine.runner_mut().barrier(old);
                    engine.poll();
                }
                assert_eq!(engine.model().phase(), phase);
                let owned = engine.lease.as_ref().unwrap().key.attempt;
                let opens = engine.runner_mut().opens.len();
                if quitting {
                    engine.quit();
                } else {
                    engine.close(engine.model().state_identity()).unwrap();
                }
                let gain = engine.gain();
                assert_eq!(
                    engine.set_gain(PlaybackGain::new(11, true).unwrap()),
                    SubmitStatus::Closing
                );
                assert_eq!(engine.gain(), gain);
                if phase == ProductPhase::ValidatingResume {
                    assert!(engine.validator.cancelled);
                    engine.validator_mut().finish(Ok(()));
                } else if phase == ProductPhase::OpeningResume {
                    engine.runner_mut().verified();
                }
                engine.runner_mut().barrier(owned);
                engine.validator.retired = quitting;
                engine.poll();
                assert_eq!(engine.runner_mut().opens.len(), opens);
                assert!(engine.model().active().is_none());
                assert_eq!(
                    engine.model().phase(),
                    if quitting {
                        ProductPhase::ShutdownReady
                    } else {
                        ProductPhase::Stopped
                    }
                );
            }
        }
    }

    #[test]
    fn gain_is_accepted_preference_without_owner_but_rejected_admission_and_blocked_cleanup_preserve_it()
     {
        let mut engine = engine();
        let initial_gain = PlaybackGain::new(80, true).unwrap();
        assert_eq!(engine.set_gain(initial_gain), SubmitStatus::Accepted);
        assert_eq!(engine.gain(), initial_gain);
        assert!(engine.runner_mut().intents.is_empty());
        let old = initial(&mut engine);
        assert_eq!(engine.runner_mut().opens.last().unwrap().2, initial_gain);
        for status in [
            SubmitStatus::NotReady,
            SubmitStatus::Closing,
            SubmitStatus::CapacityExceeded,
            SubmitStatus::StaleGeneration,
        ] {
            engine.runner_mut().immediate = Some(status);
            assert_eq!(engine.set_gain(PlaybackGain::default()), status);
            assert_eq!(engine.gain(), initial_gain);
        }
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::CleanupBlocked {
                attempt: old,
                failure: failure(
                    settings(60),
                    FailureCategory::Lifecycle(LifecycleFailure::Acknowledgement),
                    "missing ack",
                ),
            });
        engine.poll();
        assert_eq!(
            engine.set_gain(PlaybackGain::default()),
            SubmitStatus::Closing
        );
        assert_eq!(engine.gain(), initial_gain);
    }

    #[test]
    fn explicit_pause_never_resumes_and_preserves_ordered_typed_guards() {
        let mut engine = engine();
        assert_eq!(
            engine.pause(AttemptId::new(1).unwrap()),
            Err(CommandRejection::PlaybackUnavailable)
        );
        let current = initial(&mut engine);
        let stale = AttemptId::new(current.get() + 1).unwrap();
        assert_eq!(engine.pause(stale), Err(CommandRejection::StaleState));
        assert_eq!(engine.pause(current), Ok(SubmitStatus::Accepted));
        let pending = engine.model().state_identity();
        assert_eq!(engine.pause(current), Err(CommandRejection::PlaybackBusy));
        assert_eq!(
            engine.resume(pending, current),
            Err(CommandRejection::PlaybackBusy)
        );
        let PlaybackState::PausePending { request } = engine.model().active().unwrap().playback()
        else {
            panic!("admitted pause must remain pending");
        };
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::PauseObserved {
                attempt: current,
                request,
                paused: true,
            });
        engine.poll();
        let paused = engine.model().state_identity();
        let submissions = engine.runner_mut().intents.len();
        assert_eq!(engine.pause(stale), Err(CommandRejection::StaleState));
        assert_eq!(
            engine.pause(current),
            Err(CommandRejection::PlaybackUnavailable)
        );
        assert_eq!(engine.model().state_identity(), paused);
        assert!(engine.model().validation_request().is_none());
        assert_eq!(engine.runner_mut().intents.len(), submissions);
        engine.audio = Some(AudioAvailability::Silent {
            reason: AudioSilence::WaitingForSource(crate::domain::capture::AudioError::Cancelled),
        });
        assert_eq!(engine.pause(stale), Err(CommandRejection::StaleState));
        assert_eq!(
            engine.resume(paused, stale),
            Err(CommandRejection::StaleState)
        );
        assert_eq!(engine.model().state_identity(), paused);
        engine.quit();
        assert_eq!(engine.pause(current), Err(CommandRejection::ShuttingDown));
    }

    #[test]
    fn explicit_pause_rejected_admission_keeps_live_playback_eligible() {
        let mut engine = engine();
        let current = initial(&mut engine);
        let before = engine.model().state_identity();
        for status in [
            SubmitStatus::NotReady,
            SubmitStatus::Closing,
            SubmitStatus::CapacityExceeded,
            SubmitStatus::StaleGeneration,
        ] {
            engine.runner_mut().immediate = Some(status);
            assert_eq!(engine.pause(current), Ok(status));
            assert_eq!(engine.model().state_identity(), before);
            assert_eq!(
                engine.model().active().unwrap().playback(),
                PlaybackState::Live
            );
            assert!(engine.model().can_apply() && engine.model().can_restart());
            assert!(engine.model().validation_request().is_none());
        }
        assert_eq!(engine.pause(current), Ok(SubmitStatus::Accepted));
        assert!(matches!(
            engine.model().active().unwrap().playback(),
            PlaybackState::PausePending { .. }
        ));
    }

    #[test]
    fn pause_admission_is_pending_until_current_request_observed_and_rejection_is_not_paused() {
        let mut engine = engine();
        let attempt = initial(&mut engine);
        engine.runner_mut().immediate = Some(SubmitStatus::NotReady);
        assert_eq!(
            engine.toggle_pause(attempt).unwrap(),
            SubmitStatus::NotReady
        );
        assert_eq!(
            engine.model().active().unwrap().playback(),
            PlaybackState::Live
        );
        assert_eq!(
            engine.toggle_pause(attempt).unwrap(),
            SubmitStatus::Accepted
        );
        let PlaybackState::PausePending { request } = engine.model().active().unwrap().playback()
        else {
            panic!("pause must remain pending")
        };
        assert_eq!(
            engine.runner_mut().intents.last(),
            Some(&(
                attempt,
                ImmediateIntent::SetPaused {
                    request,
                    paused: true
                }
            ))
        );
        assert_eq!(
            engine.toggle_pause(attempt),
            Err(CommandRejection::PlaybackBusy)
        );
        for (received_attempt, received_request) in [
            (AttemptId::new(attempt.get() + 1).unwrap(), request),
            (attempt, PauseRequestId::new(request.get() + 1).unwrap()),
        ] {
            engine
                .runner_mut()
                .events
                .push_back(SessionEvent::PauseObserved {
                    attempt: received_attempt,
                    request: received_request,
                    paused: true,
                });
            engine.poll();
            assert_eq!(
                engine.model().active().unwrap().playback(),
                PlaybackState::PausePending { request }
            );
        }
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::PauseObserved {
                attempt,
                request,
                paused: true,
            });
        engine.poll();
        assert_eq!(
            engine.model().active().unwrap().playback(),
            PlaybackState::Paused
        );
        assert_eq!(
            engine.resume(
                engine.model().state_identity(),
                AttemptId::new(attempt.get() + 1).unwrap()
            ),
            Err(CommandRejection::StaleState)
        );
    }

    #[test]
    fn terminal_session_event_revokes_pause_and_late_readback_cannot_resurrect_it() {
        let mut engine = engine();
        let attempt = initial(&mut engine);
        engine.toggle_pause(attempt).unwrap();
        let PlaybackState::PausePending { request } = engine.model().active().unwrap().playback()
        else {
            panic!("pending")
        };
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::SessionFailed {
                attempt,
                failure: failure(settings(60), FailureCategory::Session, "pause failed"),
            });
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::PauseObserved {
                attempt,
                request,
                paused: true,
            });
        engine.poll();
        assert!(engine.model().active().is_none());
        assert_eq!(engine.runner_mut().stops.len(), 1);
    }

    #[test]
    fn audio_only_silence_keeps_pause_available_and_video_owner() {
        let mut engine = engine();
        let attempt = initial(&mut engine);
        engine.audio = Some(AudioAvailability::Silent {
            reason: AudioSilence::WaitingForSource(crate::domain::capture::AudioError::Cancelled),
        });
        assert_eq!(engine.toggle_pause(attempt), Ok(SubmitStatus::Accepted));
        assert!(matches!(
            engine.model().active().unwrap().playback(),
            PlaybackState::PausePending { .. }
        ));
        assert!(engine.runner_mut().stops.is_empty());
        assert_eq!(engine.runner_mut().opens.len(), 1);
    }

    #[test]
    fn audio_only_silence_keeps_fresh_paused_resume_available() {
        let mut engine = engine();
        let attempt = initial(&mut engine);
        confirm_pause(&mut engine, attempt);
        engine.audio = Some(AudioAvailability::Silent {
            reason: AudioSilence::WaitingForSource(crate::domain::capture::AudioError::Cancelled),
        });
        engine
            .resume(engine.model().state_identity(), attempt)
            .unwrap();
        validate(&mut engine);
        engine.runner_mut().barrier(attempt);
        engine.poll();
        engine.runner_mut().verified();
        engine.poll();
        assert_eq!(
            engine.model().active().unwrap().playback(),
            PlaybackState::Live
        );
        assert_eq!(
            engine.audio_availability(),
            Some(&AudioAvailability::Disabled)
        );
    }

    #[test]
    fn stable_paused_apply_or_restart_preserves_pause_on_rejection_and_replacement_starts_live() {
        for restart in [false, true] {
            let mut engine = engine();
            let old = initial(&mut engine);
            confirm_pause(&mut engine, old);
            engine
                .edit_draft(engine.model().draft().revision, settings(30))
                .unwrap();
            let target = if restart { settings(60) } else { settings(30) };
            for rejected in [true, false] {
                if restart {
                    engine.restart(engine.model().state_identity()).unwrap();
                } else {
                    apply(&mut engine);
                }
                assert_eq!(engine.validator.requests.last().unwrap().settings, target);
                if rejected {
                    engine.validator_mut().finish(Err(failure(
                        target.clone(),
                        FailureCategory::Validation(ValidationLayer::Mode),
                        "rejected replacement",
                    )));
                    engine.poll();
                    assert_eq!(
                        engine.model().active().unwrap().playback(),
                        PlaybackState::Paused
                    );
                    assert!(engine.runner_mut().stops.is_empty());
                } else {
                    validate(&mut engine);
                    engine.runner_mut().barrier(old);
                    engine.poll();
                    engine.runner_mut().verified();
                    engine.poll();
                    assert_eq!(
                        engine.model().active().unwrap().playback(),
                        PlaybackState::Live
                    );
                    assert_eq!(
                        engine.model().active().unwrap().applied().settings(),
                        &target
                    );
                }
            }
        }
    }

    #[test]
    fn valid_switch_validates_before_stop_and_commits_only_after_barrier_and_receipt() {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        apply(&mut engine);
        assert_eq!(engine.model().active().unwrap().attempt(), old);
        assert!(engine.runner_mut().stops.is_empty());
        validate(&mut engine);
        assert!(engine.model().active().is_none());
        assert_eq!(engine.runner_mut().opens.len(), 1);
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::OwnerStopped {
                attempt: old,
                outcome: Ok(()),
            });
        engine.poll();
        assert_eq!(engine.runner_mut().opens.len(), 1);
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::NativeReleased { attempt: old });
        engine.poll();
        assert_eq!(engine.runner_mut().opens.len(), 2);
        engine.runner_mut().verified();
        engine.poll();
        assert_eq!(
            engine.model().active().unwrap().applied().settings(),
            &settings(30)
        );
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(30)
        );
        assert_eq!(engine.model().draft().settings, settings(30));
    }

    #[test]
    fn invalid_each_installed_layer_preserves_live_attempt_and_last_valid_without_effects() {
        for layer in [
            ValidationLayer::Data,
            ValidationLayer::Identity,
            ValidationLayer::Mode,
            ValidationLayer::Input,
            ValidationLayer::Audio,
            ValidationLayer::Discovery,
        ] {
            let mut engine = engine();
            let old = initial(&mut engine);
            engine
                .edit_draft(engine.model().draft().revision, settings(30))
                .unwrap();
            apply(&mut engine);
            engine.validator_mut().finish(Err(failure(
                settings(30),
                FailureCategory::Validation(layer),
                "invalid",
            )));
            engine.poll();
            assert_eq!(engine.model().active().unwrap().attempt(), old);
            assert_eq!(
                engine.model().last_valid().unwrap().settings(),
                &settings(60)
            );
            assert_eq!(engine.model().draft().settings, settings(30));
            assert!(engine.runner_mut().stops.is_empty());
            assert_eq!(engine.runner_mut().opens.len(), 1);
            assert_eq!(
                engine
                    .model()
                    .validation_rejection()
                    .unwrap()
                    .failure
                    .category,
                FailureCategory::Validation(layer)
            );
        }
    }

    #[test]
    fn candidate_failure_waits_own_cleanup_then_freshly_restores_once_with_distinct_attempt() {
        let mut engine = engine();
        let (_, candidate) = switch(&mut engine);
        engine.runner_mut().fail("candidate refused");
        engine.poll();
        assert_eq!(engine.validator_mut().requests.len(), 2);
        engine.runner_mut().barrier(candidate);
        engine.poll();
        assert_eq!(
            engine.validator_mut().requests.last().unwrap().settings,
            settings(60)
        );
        validate(&mut engine);
        let restore = engine.runner_mut().opens.last().unwrap().0;
        assert_ne!(candidate, restore.attempt);
        assert_eq!(restore.purpose, AttemptPurpose::Restore);
        assert_eq!(restore.apply, engine.runner_mut().opens[1].0.apply);
        engine.runner_mut().verified();
        engine.poll();
        assert_eq!(
            engine.model().phase(),
            ProductPhase::ErrorWithActiveRestored
        );
        assert_eq!(
            engine.model().active().unwrap().applied().settings(),
            &settings(60)
        );
        assert_eq!(engine.model().draft().settings, settings(30));
        assert_eq!(
            engine
                .model()
                .failures()
                .unwrap()
                .candidate
                .as_ref()
                .unwrap()
                .diagnostic,
            "candidate refused"
        );
    }

    #[test]
    fn double_failure_preserves_both_causes_then_manual_reconnect_uses_last_valid_not_draft() {
        let mut engine = engine();
        let (_, candidate) = switch(&mut engine);
        engine.runner_mut().fail("candidate");
        engine.poll();
        engine.runner_mut().barrier(candidate);
        engine.poll();
        validate(&mut engine);
        let restore = engine.runner_mut().opens.last().unwrap().0.attempt;
        engine.runner_mut().fail("restore contradiction");
        engine.poll();
        assert!(!engine.model().can_reconnect());
        engine.runner_mut().barrier(restore);
        engine.poll();
        assert!(engine.model().can_reconnect());
        assert_eq!(engine.runner_mut().opens.len(), 3);
        let failures = engine.model().failures().unwrap();
        assert_eq!(failures.candidate.as_ref().unwrap().diagnostic, "candidate");
        assert_eq!(
            failures.restore.as_ref().unwrap().diagnostic,
            "restore contradiction"
        );
        engine.reconnect(engine.model().state_identity()).unwrap();
        assert_eq!(
            engine.validator_mut().requests.last().unwrap().settings,
            settings(60)
        );
        validate(&mut engine);
        engine.runner_mut().verified();
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Active);
        assert_eq!(engine.model().draft().settings, settings(30));
    }

    #[test]
    fn quit_drops_stale_results_but_drains_validation_and_owned_resource_barriers() {
        let mut engine = engine();
        let old = initial(&mut engine);
        apply(&mut engine);
        engine.quit();
        assert!(engine.validator_mut().cancelled);
        assert!(engine.validator_mut().shutdown);
        engine.runner_mut().barrier(old);
        engine.poll();
        assert!(!engine.model().shutdown_ready());
        engine.validator_mut().finish(Ok(()));
        engine.poll();
        assert!(!engine.model().shutdown_ready());
        engine.validator_mut().retired = true;
        engine.poll();
        assert!(engine.model().shutdown_ready());
        assert_eq!(engine.runner_mut().opens.len(), 1);
        assert!(engine.model().active().is_none());
    }

    #[test]
    fn incumbent_failure_during_validation_never_resurrects_on_rejection_and_success_waits_cleanup()
    {
        for valid in [false, true] {
            let mut engine = engine();
            let old = initial(&mut engine);
            apply(&mut engine);
            engine
                .runner_mut()
                .events
                .push_back(SessionEvent::SessionFailed {
                    attempt: old,
                    failure: failure(settings(60), FailureCategory::Session, "incumbent lost"),
                });
            engine.poll();
            assert!(engine.model().active().is_none());
            engine.validator_mut().finish(if valid {
                Ok(())
            } else {
                Err(failure(
                    settings(60),
                    FailureCategory::Validation(ValidationLayer::Mode),
                    "invalid candidate",
                ))
            });
            engine.poll();
            assert!(engine.model().active().is_none());
            assert!(!engine.model().can_reconnect());
            assert_eq!(engine.runner_mut().opens.len(), 1);
            engine.runner_mut().barrier(old);
            engine.poll();
            if valid {
                assert_eq!(engine.runner_mut().opens.len(), 2);
            } else {
                assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
                assert!(engine.model().can_reconnect());
                assert_eq!(
                    engine
                        .model()
                        .failures()
                        .unwrap()
                        .incumbent
                        .as_ref()
                        .unwrap()
                        .diagnostic,
                    "incumbent lost"
                );
            }
        }
    }

    #[test]
    fn accepted_gain_during_validation_survives_candidate_and_restore_rejected_gain_does_not() {
        let mut engine = engine();
        let old = initial(&mut engine);
        apply(&mut engine);
        let gain = PlaybackGain::new(73, true).unwrap();
        assert_eq!(engine.set_gain(gain), SubmitStatus::Accepted);
        engine.runner_mut().immediate = Some(SubmitStatus::CapacityExceeded);
        assert_eq!(
            engine.set_gain(PlaybackGain::default()),
            SubmitStatus::CapacityExceeded
        );
        validate(&mut engine);
        engine.runner_mut().barrier(old);
        engine.poll();
        assert_eq!(engine.runner_mut().opens.last().unwrap().2, gain);
        let candidate = engine.runner_mut().opens.last().unwrap().0.attempt;
        engine.runner_mut().fail("candidate");
        engine.poll();
        engine.runner_mut().barrier(candidate);
        engine.poll();
        validate(&mut engine);
        assert_eq!(engine.runner_mut().opens.last().unwrap().2, gain);
        assert_eq!(engine.gain(), gain);
    }

    #[test]
    fn newer_draft_and_wrong_receipt_cannot_commit_or_overwrite_last_valid() {
        let mut engine = engine();
        let (_, candidate) = switch(&mut engine);
        engine
            .edit_draft(engine.model().draft().revision, settings(25))
            .unwrap();
        let key = engine.runner_mut().opens.last().unwrap().0;
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::OpenVerified {
                key,
                receipt: Box::new(receipt(settings(60))),
            });
        engine.poll();
        assert!(engine.model().active().is_none());
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
        engine.runner_mut().barrier(candidate);
        engine.poll();
        validate(&mut engine);
        engine.runner_mut().verified();
        engine.poll();
        assert_eq!(engine.model().draft().settings, settings(25));
    }

    #[test]
    fn no_history_no_resource_spawn_failure_never_rolls_back_startup_config() {
        let mut engine = engine();
        engine.runner_mut().start_failure = Some(StartFailure::NoResourcesCreated(failure(
            settings(60),
            FailureCategory::Session,
            "spawn refused",
        )));
        apply(&mut engine);
        validate(&mut engine);
        assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
        assert!(engine.model().last_valid().is_none());
        assert!(!engine.model().can_reconnect());
        assert_eq!(engine.validator_mut().requests.len(), 1);
        assert!(engine.runner_mut().stops.is_empty());
    }

    #[test]
    fn missing_ack_or_release_blocks_next_owner_and_early_release_is_not_proof() {
        let mut engine = engine();
        let old = initial(&mut engine);
        apply(&mut engine);
        validate(&mut engine);
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::NativeReleased { attempt: old });
        engine.poll();
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::OwnerStopped {
                attempt: old,
                outcome: Ok(()),
            });
        engine.poll();
        assert_eq!(engine.runner_mut().opens.len(), 1);
        assert!(!engine.model().can_reconnect());
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::NativeReleased { attempt: old });
        engine.poll();
        assert_eq!(engine.runner_mut().opens.len(), 2);
    }
    fn reach_step(step: usize) -> Engine {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        apply(&mut engine);
        if step == 0 {
            return engine;
        }
        validate(&mut engine);
        if step == 1 {
            return engine;
        }
        engine.runner_mut().barrier(old);
        engine.poll();
        if step == 2 {
            return engine;
        }
        let candidate = engine.runner_mut().opens.last().unwrap().0.attempt;
        engine.runner_mut().fail("candidate");
        engine.poll();
        if step == 3 {
            return engine;
        }
        engine.runner_mut().barrier(candidate);
        engine.poll();
        if step == 4 {
            return engine;
        }
        validate(&mut engine);
        if step == 5 {
            return engine;
        }
        engine.runner_mut().fail("restore");
        engine.poll();
        engine
    }

    #[test]
    fn overlapping_apply_rejected_at_every_candidate_and_restore_step_without_new_effect_or_id() {
        for step in 0..7 {
            let mut engine = reach_step(step);
            let identity = engine.model().state_identity();
            let requests = engine.validator_mut().requests.len();
            let opens = engine.runner_mut().opens.len();
            let stops = engine.runner_mut().stops.len();
            assert_eq!(
                engine.apply(identity, engine.model().draft().revision),
                Err(CommandRejection::ApplyInProgress)
            );
            assert_eq!(engine.model().state_identity(), identity);
            assert_eq!(engine.validator_mut().requests.len(), requests);
            assert_eq!(engine.runner_mut().opens.len(), opens);
            assert_eq!(engine.runner_mut().stops.len(), stops);
        }
    }

    #[test]
    fn close_and_quit_supersede_every_step_and_quit_dominates_close() {
        for step in 0..7 {
            for quit in [false, true] {
                let mut engine = reach_step(step);
                let opens = engine.runner_mut().opens.len();
                engine.close(engine.model().state_identity()).unwrap();
                if quit {
                    engine.quit();
                }
                assert!(engine.model().active().is_none());
                if let Some(lease) = &engine.lease {
                    let key = lease.key;
                    let stale = receipt(lease.settings.clone());
                    engine
                        .runner_mut()
                        .events
                        .push_back(SessionEvent::OpenVerified {
                            key,
                            receipt: Box::new(stale),
                        });
                    engine.runner_mut().barrier(key.attempt);
                }
                if engine.validator_mut().request.is_some() {
                    engine.validator_mut().finish(Ok(()));
                }
                engine.poll();
                if quit {
                    engine.validator_mut().retired = true;
                    engine.poll();
                    assert!(engine.model().shutdown_ready());
                    assert_eq!(
                        engine.apply(
                            engine.model().state_identity(),
                            engine.model().draft().revision
                        ),
                        Err(CommandRejection::ShuttingDown)
                    );
                } else {
                    assert_eq!(engine.model().phase(), ProductPhase::Stopped);
                }
                assert_eq!(engine.runner_mut().opens.len(), opens);
                assert_eq!(
                    engine.model().last_valid().unwrap().settings(),
                    &settings(60)
                );
                assert_eq!(engine.model().draft().settings, settings(30));
            }
        }
    }

    #[test]
    fn late_candidate_same_apply_cannot_corrupt_restore_and_unknown_or_duplicate_cleanup_ignored() {
        let mut engine = reach_step(5);
        let candidate = engine.runner_mut().opens[1].0;
        let restore = engine.runner_mut().opens[2].0;
        assert_eq!(candidate.apply, restore.apply);
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::OpenVerified {
                key: candidate,
                receipt: Box::new(receipt(settings(30))),
            });
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::OpenFailed {
                key: candidate,
                failure: failure(settings(30), FailureCategory::Session, "late candidate"),
            });
        engine.runner_mut().barrier(candidate.attempt);
        engine.runner_mut().barrier(AttemptId::new(999).unwrap());
        engine.poll();
        assert_eq!(engine.model().opening().unwrap().0, restore);
        assert!(engine.model().active().is_none());
        assert_eq!(
            engine
                .model()
                .failures()
                .unwrap()
                .candidate
                .as_ref()
                .unwrap()
                .diagnostic,
            "candidate"
        );
        engine.runner_mut().verified();
        engine.poll();
        assert_eq!(
            engine.model().phase(),
            ProductPhase::ErrorWithActiveRestored
        );
    }

    #[test]
    fn ended_or_unexpected_owner_ack_during_validation_revoke_incumbent_for_success_and_rejection()
    {
        for unexpected_ack in [false, true] {
            for valid in [false, true] {
                let mut engine = engine();
                let old = initial(&mut engine);
                apply(&mut engine);
                engine.runner_mut().events.push_back(if unexpected_ack {
                    SessionEvent::OwnerStopped {
                        attempt: old,
                        outcome: Ok(()),
                    }
                } else {
                    SessionEvent::StreamEnded {
                        attempt: old,
                        reason: 0,
                        error: 0,
                    }
                });
                engine.poll();
                assert!(engine.model().active().is_none());
                engine.validator_mut().finish(if valid {
                    Ok(())
                } else {
                    Err(failure(
                        settings(60),
                        FailureCategory::Validation(ValidationLayer::Audio),
                        "invalid audio",
                    ))
                });
                engine.poll();
                assert!(engine.model().active().is_none());
                assert_eq!(engine.runner_mut().opens.len(), 1);
                if !unexpected_ack {
                    engine
                        .runner_mut()
                        .events
                        .push_back(SessionEvent::OwnerStopped {
                            attempt: old,
                            outcome: Ok(()),
                        });
                }
                engine
                    .runner_mut()
                    .events
                    .push_back(SessionEvent::NativeReleased { attempt: old });
                engine.poll();
                if valid {
                    assert_eq!(engine.runner_mut().opens.len(), 2);
                } else {
                    assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
                    assert!(engine.model().failures().unwrap().incumbent.is_some());
                    assert!(engine.model().validation_rejection().is_some());
                    assert!(engine.model().can_reconnect());
                }
            }
        }
    }

    #[test]
    fn owner_ack_beats_cached_ready_even_if_adapter_orders_ready_first() {
        let mut engine = engine();
        apply(&mut engine);
        validate(&mut engine);
        let key = engine.runner_mut().opens[0].0;
        engine.runner_mut().verified();
        engine.runner_mut().barrier(key.attempt);
        engine.poll();
        assert!(engine.model().active().is_none());
        assert!(engine.model().last_valid().is_none());
        assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
    }

    #[test]
    fn enabled_audio_receipt_requires_exact_source_active_disabled_cannot_commit() {
        use crate::domain::capture::AudioSourceIdentity;
        for outcome in 0..3 {
            let source = AudioSourceIdentity::new("selected".into(), vec![]).unwrap();
            let mut selected = settings(60);
            selected.audio = AudioSelection::Enabled {
                source: source.clone(),
            };
            let mut engine = ApplyCoordinator::new(
                selected.clone(),
                PlaybackGain::default(),
                Validator::default(),
                Runner::default(),
            );
            apply(&mut engine);
            validate(&mut engine);
            let key = engine.runner_mut().opens[0].0;
            let mut opened = receipt(selected);
            let route = route_fixture(
                key.attempt,
                AudioEpoch::new(1).unwrap(),
                engine.lease.as_ref().unwrap().watch,
                source.clone(),
            );
            opened.audio = match outcome {
                0 => AudioOutcome::Disabled,
                1 => AudioOutcome::Active {
                    route: route_fixture(
                        key.attempt,
                        AudioEpoch::new(1).unwrap(),
                        engine.lease.as_ref().unwrap().watch,
                        AudioSourceIdentity::new("wrong".into(), vec![]).unwrap(),
                    ),
                },
                _ => AudioOutcome::Active {
                    route: route.clone(),
                },
            };
            engine
                .runner_mut()
                .events
                .push_back(SessionEvent::AudioAvailability {
                    attempt: key.attempt,
                    status: AudioAvailability::Active { route },
                });
            engine
                .runner_mut()
                .events
                .push_back(SessionEvent::OpenVerified {
                    key,
                    receipt: Box::new(opened),
                });
            engine.poll();
            assert_eq!(engine.model().active().is_some(), outcome == 2);
            assert_eq!(engine.model().last_valid().is_some(), outcome == 2);
        }
    }

    #[test]
    fn unsafe_audio_retirement_before_receipt_fails_candidate_but_missing_video_observations_remain_unverified()
     {
        let mut engine = engine();
        apply(&mut engine);
        validate(&mut engine);
        let key = engine.runner_mut().opens[0].0;
        engine.runner_mut().verified();
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::SessionFailed {
                attempt: key.attempt,
                failure: failure(
                    settings(60),
                    FailureCategory::Lifecycle(LifecycleFailure::Protocol),
                    "audio demux retirement could not be proven",
                ),
            });
        engine.poll();
        assert!(engine.model().active().is_none());
        let mut engine = self::engine();
        apply(&mut engine);
        validate(&mut engine);
        let key = engine.runner_mut().opens[0].0;
        let mut opened = receipt(settings(60));
        opened.verification.decoded_size = FactStatus::Unverified;
        opened.verification.nominal_rate = FactStatus::Unverified;
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::OpenVerified {
                key,
                receipt: Box::new(opened),
            });
        engine.poll();
        assert!(engine.model().active().is_some());
    }

    #[test]
    fn terminal_submission_errors_leave_incumbent_intact_and_validation_result_identity_checked() {
        for rejection in [
            SubmitFailure::CapacityUnavailable,
            SubmitFailure::Disconnected,
        ] {
            let mut engine = engine();
            let old = initial(&mut engine);
            engine.validator_mut().reject = Some(rejection);
            let result = engine.apply(
                engine.model().state_identity(),
                engine.model().draft().revision,
            );
            assert_eq!(
                result,
                Err(if rejection == SubmitFailure::Disconnected {
                    CommandRejection::Disconnected
                } else {
                    CommandRejection::CapacityUnavailable
                })
            );
            assert_eq!(engine.model().active().unwrap().attempt(), old);
            assert!(engine.runner_mut().stops.is_empty());
        }
        let mut engine = engine();
        let old = initial(&mut engine);
        apply(&mut engine);
        let mut request = engine.validator_mut().request.take().unwrap();
        request.settings = settings(30);
        engine.validator_mut().result = Some(ValidationResult {
            stamp: request.watch,
            result: ValidationOutcome::Prepared(TestPrepared {
                settings: settings(30),
                stamp: request.watch,
            }),
            request,
        });
        engine.poll();
        assert_eq!(engine.model().active().unwrap().attempt(), old);
        assert!(engine.runner_mut().stops.is_empty());
        assert_eq!(
            engine
                .model()
                .validation_rejection()
                .unwrap()
                .failure
                .category,
            FailureCategory::Lifecycle(LifecycleFailure::Protocol)
        );
    }

    #[test]
    fn cleanup_failure_appends_without_erasing_primary_and_poisoned_barrier_never_reopens() {
        let mut engine = reach_step(3);
        let candidate = engine.runner_mut().opens[1].0.attempt;
        let cleanup = failure(
            settings(30),
            FailureCategory::Lifecycle(LifecycleFailure::Quiescence),
            "guard error",
        );
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::OwnerStopped {
                attempt: candidate,
                outcome: Err(cleanup),
            });
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::NativeReleased { attempt: candidate });
        engine.poll();
        assert_eq!(
            engine
                .model()
                .failures()
                .unwrap()
                .candidate
                .as_ref()
                .unwrap()
                .diagnostic,
            "candidate"
        );
        assert_eq!(
            engine.model().failures().unwrap().cleanup[0].diagnostic,
            "guard error"
        );
        validate(&mut engine);
        let restore = engine.runner_mut().opens.last().unwrap().0.attempt;
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::CleanupBlocked {
                attempt: restore,
                failure: failure(
                    settings(60),
                    FailureCategory::Lifecycle(LifecycleFailure::Acknowledgement),
                    "missing ack",
                ),
            });
        engine.runner_mut().barrier(restore);
        engine.poll();
        assert!(matches!(
            engine.model().cleanup(),
            CleanupStatus::Blocked { .. }
        ));
        assert!(!engine.model().can_reconnect());
        assert!(engine.model().active().is_none());
        assert_eq!(engine.runner_mut().opens.len(), 3);
    }

    #[test]
    fn partial_spawn_failure_waits_real_barrier_and_failed_restore_validation_never_spawns_restore()
    {
        let mut engine = engine();
        let old = initial(&mut engine);
        apply(&mut engine);
        validate(&mut engine);
        engine.runner_mut().start_failure = Some(StartFailure::ResourcesCreated(failure(
            settings(60),
            FailureCategory::Lifecycle(LifecycleFailure::OwnerSpawn),
            "partial spawn",
        )));
        engine.runner_mut().barrier(old);
        engine.poll();
        let candidate = engine.runner_mut().opens[1].0.attempt;
        assert_eq!(engine.validator_mut().requests.len(), 2);
        engine.runner_mut().barrier(candidate);
        engine.poll();
        engine.validator_mut().finish(Err(failure(
            settings(60),
            FailureCategory::Validation(ValidationLayer::Identity),
            "prior disappeared",
        )));
        engine.poll();
        assert_eq!(engine.runner_mut().opens.len(), 2);
        assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
        assert_eq!(
            engine
                .model()
                .failures()
                .unwrap()
                .candidate
                .as_ref()
                .unwrap()
                .diagnostic,
            "partial spawn"
        );
        assert_eq!(
            engine
                .model()
                .failures()
                .unwrap()
                .restore
                .as_ref()
                .unwrap()
                .diagnostic,
            "prior disappeared"
        );
    }

    #[test]
    fn close_during_validation_blocks_replacement_until_terminal_drain_and_stale_audio_cannot_replace_diagnostic()
     {
        let mut engine = engine();
        let old = initial(&mut engine);
        apply(&mut engine);
        engine.close(engine.model().state_identity()).unwrap();
        engine.runner_mut().barrier(old);
        engine.poll();
        assert_eq!(
            engine.apply(
                engine.model().state_identity(),
                engine.model().draft().revision
            ),
            Err(CommandRejection::CleanupIncomplete)
        );
        assert!(!engine.gain_admission_open());
        let preference = engine.gain();
        assert_eq!(
            engine.set_gain(PlaybackGain::new(80, true).unwrap()),
            SubmitStatus::Closing
        );
        assert_eq!(engine.gain(), preference);
        engine.validator_mut().finish(Ok(()));
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Stopped);
        assert!(engine.gain_admission_open());
        engine.reconnect(engine.model().state_identity()).unwrap();
        validate(&mut engine);
        let current = engine.runner_mut().opens.last().unwrap().0.attempt;
        engine.runner_mut().verified();
        engine.poll();
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt: old,
                status: AudioAvailability::Silent {
                    reason: AudioSilence::WaitingForSource(
                        crate::domain::capture::AudioError::Cancelled,
                    ),
                },
            });
        engine.poll();
        assert_eq!(
            engine.audio_availability(),
            Some(&AudioAvailability::Disabled)
        );
        assert_eq!(engine.model().active().unwrap().attempt(), current);
    }
    #[test]
    fn old_stop_cleanup_evidence_survives_successful_candidate_commit() {
        let mut engine = reach_step(1);
        let old = engine.runner_mut().opens[0].0.attempt;
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::OwnerStopped {
                attempt: old,
                outcome: Err(failure(
                    settings(60),
                    FailureCategory::Lifecycle(LifecycleFailure::Quiescence),
                    "old cleanup evidence",
                )),
            });
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::NativeReleased { attempt: old });
        engine.poll();
        engine.runner_mut().verified();
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Active);
        assert_eq!(
            engine.model().failures().unwrap().cleanup[0].diagnostic,
            "old cleanup evidence"
        );
    }
    #[test]
    fn close_and_quit_retain_terminal_cleanup_error_and_existing_candidate_failure() {
        for failed_candidate in [false, true] {
            for quit in [false, true] {
                let mut engine = if failed_candidate {
                    reach_step(3)
                } else {
                    self::engine()
                };
                if !failed_candidate {
                    initial(&mut engine);
                }
                let attempt = engine.lease.as_ref().unwrap().key.attempt;
                let requested = engine.lease.as_ref().unwrap().settings.clone();
                engine.close(engine.model().state_identity()).unwrap();
                if quit {
                    engine.quit();
                }
                engine
                    .runner_mut()
                    .events
                    .push_back(SessionEvent::OwnerStopped {
                        attempt,
                        outcome: Err(failure(
                            requested.clone(),
                            FailureCategory::Lifecycle(LifecycleFailure::Quiescence),
                            "terminal cleanup error",
                        )),
                    });
                engine.poll();
                assert_eq!(engine.model().phase(), ProductPhase::Stopping);
                assert_eq!(
                    engine.model().failures().unwrap().cleanup[0].diagnostic,
                    "terminal cleanup error"
                );
                engine
                    .runner_mut()
                    .events
                    .push_back(SessionEvent::OwnerStopped {
                        attempt,
                        outcome: Err(failure(
                            requested,
                            FailureCategory::Lifecycle(LifecycleFailure::Quiescence),
                            "duplicate ack error",
                        )),
                    });
                engine
                    .runner_mut()
                    .events
                    .push_back(SessionEvent::NativeReleased { attempt });
                engine.validator_mut().retired = quit;
                engine.poll();
                assert_eq!(
                    engine.model().phase(),
                    if quit {
                        ProductPhase::ShutdownReady
                    } else {
                        ProductPhase::Stopped
                    }
                );
                let report = engine.model().failures().unwrap();
                assert_eq!(report.cleanup.len(), 1);
                assert_eq!(report.cleanup[0].diagnostic, "terminal cleanup error");
                assert_eq!(report.candidate.is_some(), failed_candidate);
                if failed_candidate {
                    assert_eq!(report.candidate.as_ref().unwrap().diagnostic, "candidate");
                }
            }
        }
    }
    fn poison_active(engine: &mut Engine, attempt: AttemptId) {
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::SessionFailed {
                attempt,
                failure: failure(
                    settings(60),
                    FailureCategory::Lifecycle(LifecycleFailure::SurfaceLoss),
                    "surface lost",
                ),
            });
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::CleanupBlocked {
                attempt,
                failure: failure(
                    settings(60),
                    FailureCategory::Lifecycle(LifecycleFailure::SurfaceLoss),
                    "native lifetime poisoned",
                ),
            });
        engine.poll();
    }

    #[test]
    fn poisoned_real_barrier_retires_resources_without_reopen_and_permits_failure_quit() {
        let mut engine = engine();
        let old = initial(&mut engine);
        poison_active(&mut engine, old);
        engine.runner_mut().barrier(old);
        engine.poll();
        assert!(engine.lease.is_none());
        assert!(matches!(
            engine.model().cleanup(),
            CleanupStatus::Blocked { .. }
        ));
        assert!(engine.model().active().is_none());
        assert!(!engine.model().can_reconnect());
        engine.close(engine.model().state_identity()).unwrap();
        assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
        assert_eq!(
            engine.reconnect(engine.model().state_identity()),
            Err(CommandRejection::CleanupBlocked)
        );
        assert_eq!(
            engine.apply(
                engine.model().state_identity(),
                engine.model().draft().revision
            ),
            Err(CommandRejection::CleanupBlocked)
        );
        engine.quit();
        assert!(!engine.model().shutdown_ready());
        engine.validator_mut().retired = true;
        engine.poll();
        assert!(engine.model().shutdown_ready());
        assert!(matches!(
            engine.model().cleanup(),
            CleanupStatus::Blocked { .. }
        ));
        assert_eq!(
            engine.model().failures().unwrap().cleanup[0].diagnostic,
            "native lifetime poisoned"
        );
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
        assert_eq!(engine.runner_mut().opens.len(), 1);
    }

    #[test]
    fn poisoned_shutdown_missing_ack_or_release_never_invents_retirement() {
        for missing_ack in [false, true] {
            let mut engine = engine();
            let old = initial(&mut engine);
            poison_active(&mut engine, old);
            engine.quit();
            engine.validator_mut().retired = true;
            engine.runner_mut().events.push_back(if missing_ack {
                SessionEvent::NativeReleased { attempt: old }
            } else {
                SessionEvent::OwnerStopped {
                    attempt: old,
                    outcome: Ok(()),
                }
            });
            engine.poll();
            assert!(!engine.model().shutdown_ready());
            assert!(engine.lease.is_some());
            assert!(matches!(
                engine.model().cleanup(),
                CleanupStatus::Blocked { .. }
            ));
            assert_eq!(engine.runner_mut().opens.len(), 1);
            if missing_ack {
                engine
                    .runner_mut()
                    .events
                    .push_back(SessionEvent::OwnerStopped {
                        attempt: old,
                        outcome: Ok(()),
                    });
                engine.poll();
                assert!(!engine.model().shutdown_ready());
            }
            engine
                .runner_mut()
                .events
                .push_back(SessionEvent::NativeReleased { attempt: old });
            engine.poll();
            assert!(engine.model().shutdown_ready());
        }
    }

    #[test]
    fn poisoned_quit_still_drains_cancelled_validation_after_actual_resource_retirement() {
        let mut engine = engine();
        let old = initial(&mut engine);
        apply(&mut engine);
        poison_active(&mut engine, old);
        engine.runner_mut().barrier(old);
        engine.poll();
        engine.quit();
        engine.validator_mut().retired = true;
        engine.poll();
        assert!(!engine.model().shutdown_ready());
        engine.validator_mut().finish(Ok(()));
        engine.poll();
        assert!(engine.model().shutdown_ready());
        assert_eq!(engine.runner_mut().opens.len(), 1);
        assert!(engine.model().active().is_none());
    }
    #[test]
    fn poisoned_lease_retains_distinct_missing_ack_evidence_without_replacing_first_poison() {
        let mut engine = engine();
        let old = initial(&mut engine);
        poison_active(&mut engine, old);
        let first = engine.model().cleanup().clone();
        let acknowledgement = failure(
            settings(60),
            FailureCategory::Lifecycle(LifecycleFailure::Acknowledgement),
            "terminal acknowledgement channel disconnected",
        );
        for _ in 0..2 {
            engine
                .runner_mut()
                .events
                .push_back(SessionEvent::CleanupBlocked {
                    attempt: old,
                    failure: acknowledgement.clone(),
                });
        }
        engine.quit();
        engine.validator_mut().retired = true;
        engine.poll();
        assert!(!engine.model().shutdown_ready());
        assert_eq!(engine.model().cleanup(), &first);
        let report = engine.model().failures().unwrap();
        assert_eq!(report.cleanup.len(), 2);
        assert_eq!(report.cleanup[0].diagnostic, "native lifetime poisoned");
        assert_eq!(report.cleanup[1], acknowledgement);
        assert!(report.incumbent.is_some());
        assert!(!engine.model().can_reconnect());
        assert_eq!(engine.runner_mut().opens.len(), 1);
    }
    fn observation(
        engine: &Engine,
        epoch: u64,
        video: crate::domain::capture::VideoPresence,
        audio: SourcePresence,
        removal: Option<u64>,
    ) -> RecoveryObservation {
        RecoveryObservation {
            stamp: WatchStamp {
                watch: engine.model().watch_target().unwrap().watch,
                epoch: crate::domain::capture::ObservationEpoch::new(epoch).unwrap(),
            },
            video,
            audio,
            last_video_removal: removal
                .map(|value| crate::domain::capture::ObservationEpoch::new(value).unwrap()),
        }
    }
    fn observe(engine: &mut Engine, observation: RecoveryObservation) {
        engine.validator.observation = Some(observation);
        engine.poll();
    }
    fn eof(engine: &mut Engine, attempt: AttemptId) {
        engine.runner.events.push_back(SessionEvent::StreamEnded {
            attempt,
            reason: 0,
            error: 0,
        });
        engine.poll();
    }
    fn recovered(engine: &mut Engine, old: AttemptId) -> AttemptId {
        engine.runner.barrier(old);
        engine.poll();
        validate(engine);
        engine.runner.verified();
        engine.poll();
        engine.model().active().unwrap().attempt()
    }
    #[test]
    fn initial_open_waits_for_authoritative_subscription_scan() {
        let mut engine = engine();
        engine.validator.delay_initial = true;
        apply(&mut engine);
        assert_eq!(engine.model().phase(), ProductPhase::Validating);
        assert!(engine.validator.requests.is_empty());
        assert!(engine.runner.opens.is_empty());
        engine.poll();
        assert!(engine.validator.requests.is_empty());
        let current = observation(
            &engine,
            1,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Disabled,
            None,
        );
        observe(&mut engine, current);
        assert_eq!(engine.validator.requests.len(), 1);
        validate(&mut engine);
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Active);
    }
    #[test]
    fn eof_alone_freezes_saved_state_and_removal_upgrades_without_duplicate_stop() {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        eof(&mut engine, old);
        assert_eq!(engine.model().phase(), ProductPhase::Disconnected);
        let loss = engine.model().recovery().unwrap();
        assert_eq!(loss.applied.settings(), &settings(60));
        assert_eq!(
            loss.evidence,
            LossEvidence::StreamEnded {
                reason: 0,
                error: 0
            }
        );
        let pending = engine.model().state_identity().operation().unwrap();
        assert_eq!(
            engine.reconnect(engine.model().state_identity()),
            Ok(ReconnectAdmission::Joined(pending))
        );
        let real_failure = ApplyFailure::new(
            FailureCategory::Session,
            Stage::StreamStart,
            Cause::Generic,
            settings(60),
            "backend_end",
            "genuine structured media termination",
        );
        engine.runner.events.push_back(SessionEvent::SessionFailed {
            attempt: old,
            failure: real_failure.clone(),
        });
        let removed = observation(
            &engine,
            2,
            crate::domain::capture::VideoPresence::Absent,
            SourcePresence::Disabled,
            Some(2),
        );
        observe(&mut engine, removed.clone());
        assert_eq!(engine.model().recovery().unwrap().failure, real_failure);
        assert_eq!(
            engine.model().recovery().unwrap().evidence,
            LossEvidence::Removed {
                stamp: removed.stamp
            }
        );
        assert_eq!(engine.runner.stops.len(), 1);
        engine.runner.barrier(old);
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Disconnected);
        assert_eq!(engine.runner.opens.len(), 1);
        let returned = observation(
            &engine,
            3,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Disabled,
            Some(2),
        );
        observe(&mut engine, returned.clone());
        assert_eq!(
            engine.validator.request.as_ref().unwrap().settings,
            settings(60)
        );
        assert_eq!(
            engine.validator.request.as_ref().unwrap().key.apply,
            pending
        );
        observe(&mut engine, returned);
        assert_eq!(engine.validator.requests.len(), 2);
        validate(&mut engine);
        engine.runner.verified();
        engine.poll();
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
        assert_eq!(engine.model().draft().settings, settings(30));
        assert_eq!(
            engine.reconnect(engine.model().state_identity()),
            Ok(ReconnectAdmission::HealthyNoop)
        );
        assert_eq!(engine.runner.opens.len(), 2);
    }
    #[test]
    fn removal_alone_with_coalesced_return_revokes_live_and_requires_both_barriers() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let returned = observation(
            &engine,
            3,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Disabled,
            Some(2),
        );
        observe(&mut engine, returned);
        assert!(engine.model().active().is_none());
        assert!(matches!(
            engine.model().recovery().unwrap().evidence,
            LossEvidence::Removed { .. }
        ));
        assert_eq!(engine.runner.stops.len(), 1);
        engine
            .runner
            .events
            .push_back(SessionEvent::NativeReleased { attempt: old });
        engine.poll();
        assert_eq!(engine.validator.requests.len(), 1);
        engine.runner.events.push_back(SessionEvent::OwnerStopped {
            attempt: old,
            outcome: Ok(()),
        });
        engine.poll();
        assert_eq!(engine.validator.requests.len(), 1);
        engine
            .runner
            .events
            .push_back(SessionEvent::NativeReleased { attempt: old });
        engine.poll();
        assert_eq!(engine.validator.requests.len(), 2);
        validate(&mut engine);
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Active);
        assert_eq!(engine.runner.opens.len(), 2);
    }
    #[test]
    fn coalesced_removal_invalidates_validation_and_drains_its_old_terminal_result() {
        let mut engine = engine();
        let old = initial(&mut engine);
        eof(&mut engine, old);
        engine.runner.barrier(old);
        engine.poll();
        let invalidated = engine.validator.request.as_ref().unwrap().clone();
        let returned = observation(
            &engine,
            3,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Disabled,
            Some(2),
        );
        observe(&mut engine, returned);
        assert!(engine.validator.cancelled);
        assert_eq!(engine.runner.opens.len(), 1);
        assert_eq!(engine.validator.requests.len(), 2);
        engine.validator.finish(Ok(()));
        engine.poll();
        assert_eq!(engine.runner.opens.len(), 1);
        let replacement = engine.validator.request.as_ref().unwrap();
        assert_eq!(replacement.key.apply, invalidated.key.apply);
        assert_eq!(replacement.watch.epoch.get(), 3);
        assert_eq!(engine.validator.requests.len(), 3);
        validate(&mut engine);
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.runner.opens.len(), 2);
        assert_eq!(engine.model().phase(), ProductPhase::Active);
    }
    #[test]
    fn removal_beats_cached_open_readiness_and_retired_attempts_cannot_resurrect() {
        let mut engine = engine();
        let old = initial(&mut engine);
        eof(&mut engine, old);
        engine.runner.barrier(old);
        engine.poll();
        validate(&mut engine);
        let first = engine.runner.opens.last().unwrap().0;
        engine.runner.verified();
        let returned = observation(
            &engine,
            3,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Disabled,
            Some(2),
        );
        observe(&mut engine, returned);
        assert!(engine.model().active().is_none());
        assert_eq!(engine.runner.stops.len(), 2);
        let next = recovered(&mut engine, first.attempt);
        assert_ne!(next, first.attempt);
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key: first,
            receipt: Box::new(receipt(settings(60))),
        });
        engine.runner.events.push_back(SessionEvent::StreamEnded {
            attempt: old,
            reason: 0,
            error: 0,
        });
        engine.runner.barrier(first.attempt);
        engine.poll();
        assert_eq!(engine.model().active().unwrap().attempt(), next);
        assert_eq!(engine.runner.opens.len(), 3);
    }
    #[test]
    fn obsolete_watch_and_epoch_cannot_invalidate_current_video() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let wrong = RecoveryObservation {
            stamp: WatchStamp {
                watch: crate::domain::capture::WatchId::new(99).unwrap(),
                epoch: crate::domain::capture::ObservationEpoch::new(99).unwrap(),
            },
            video: crate::domain::capture::VideoPresence::Absent,
            audio: SourcePresence::Disabled,
            last_video_removal: Some(crate::domain::capture::ObservationEpoch::new(99).unwrap()),
        };
        observe(&mut engine, wrong);
        let current = observation(
            &engine,
            4,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Disabled,
            None,
        );
        observe(&mut engine, current);
        let stale = observation(
            &engine,
            2,
            crate::domain::capture::VideoPresence::Absent,
            SourcePresence::Disabled,
            Some(2),
        );
        observe(&mut engine, stale);
        assert_eq!(engine.model().active().unwrap().attempt(), old);
        assert!(engine.runner.stops.is_empty());
    }
    #[test]
    fn explicit_current_choice_is_one_use_and_commits_only_fresh_validated_complete_identity() {
        use crate::domain::capture::{CandidateId, RecoveryCandidate, VideoPresence};
        let mut engine = engine();
        let old = initial(&mut engine);
        eof(&mut engine, old);
        let mut ambiguous = observation(
            &engine,
            2,
            VideoPresence::Absent,
            SourcePresence::Disabled,
            None,
        );
        let token = SelectionToken {
            stamp: ambiguous.stamp,
            candidate: CandidateId::new(1).unwrap(),
        };
        let identity = DeviceIdentity::new(
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
        ambiguous.video = VideoPresence::Ambiguous(vec![RecoveryCandidate {
            token,
            identity: identity.clone(),
            description: "physical candidate".into(),
        }]);
        observe(&mut engine, ambiguous);
        engine.runner.barrier(old);
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::SelectionRequired);
        let stale = SelectionToken {
            stamp: WatchStamp {
                watch: token.stamp.watch,
                epoch: crate::domain::capture::ObservationEpoch::new(1).unwrap(),
            },
            candidate: token.candidate,
        };
        assert_eq!(
            engine.choose_recovery(engine.model().state_identity(), stale),
            Err(CommandRejection::SelectionUnavailable)
        );
        engine
            .choose_recovery(engine.model().state_identity(), token)
            .unwrap();
        assert_eq!(
            engine.choose_recovery(engine.model().state_identity(), token),
            Err(CommandRejection::ApplyInProgress)
        );
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
        let mut fresh = observation(
            &engine,
            3,
            VideoPresence::Absent,
            SourcePresence::Disabled,
            None,
        );
        fresh.video = VideoPresence::Ambiguous(vec![RecoveryCandidate {
            token: SelectionToken {
                stamp: fresh.stamp,
                candidate: token.candidate,
            },
            identity: identity.clone(),
            description: "physical candidate".into(),
        }]);
        observe(&mut engine, fresh.clone());
        assert_eq!(engine.model().phase(), ProductPhase::Recovering);
        assert!(!engine.validator.cancelled);
        let request = engine.validator.request.take().unwrap();
        let mut selected = request.settings.clone();
        selected.video.identity = identity.clone();
        engine.validator.result = Some(ValidationResult {
            stamp: fresh.stamp,
            result: ValidationOutcome::Prepared(TestPrepared {
                settings: selected.clone(),
                stamp: fresh.stamp,
            }),
            request,
        });
        engine.poll();
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
        let mut after_open = observation(
            &engine,
            4,
            VideoPresence::Absent,
            SourcePresence::Disabled,
            None,
        );
        after_open.video = VideoPresence::Ambiguous(vec![RecoveryCandidate {
            token: SelectionToken {
                stamp: after_open.stamp,
                candidate: token.candidate,
            },
            identity: identity.clone(),
            description: "physical candidate".into(),
        }]);
        observe(&mut engine, after_open);
        assert!(engine.model().opening().is_some());
        assert_eq!(engine.runner.stops.len(), 1);
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.model().last_valid().unwrap().settings(), &selected);
        assert_eq!(engine.model().draft().settings, settings(60));
        assert_eq!(
            engine.model().watch_target().unwrap().video.identity,
            identity
        );
        assert_ne!(
            engine.model().watch_target().unwrap().watch,
            token.stamp.watch
        );
    }
    #[test]
    fn admitted_and_confirmed_pause_restore_paused_but_unadmitted_reservation_does_not() {
        for admission in 0..3 {
            let mut engine = engine();
            let old = initial(&mut engine);
            if admission == 0 {
                engine.model.prepare_pause(old).unwrap();
            } else if admission == 1 {
                assert_eq!(engine.pause(old), Ok(SubmitStatus::Accepted));
            } else {
                confirm_pause(&mut engine, old);
            }
            eof(&mut engine, old);
            let expected = if admission == 0 {
                InitialPlayback::Live
            } else {
                InitialPlayback::Paused
            };
            assert_eq!(engine.model().recovery().unwrap().playback, expected);
            let current = recovered(&mut engine, old);
            assert_eq!(*engine.runner.playback.last().unwrap(), expected);
            assert_eq!(
                engine.model().active().unwrap().playback(),
                if admission == 0 {
                    PlaybackState::Live
                } else {
                    PlaybackState::Paused
                }
            );
            if admission != 0 {
                engine
                    .resume(engine.model().state_identity(), current)
                    .unwrap();
                validate(&mut engine);
                engine.runner.barrier(current);
                engine.poll();
                engine.runner.verified();
                engine.poll();
                assert_eq!(
                    engine.model().active().unwrap().playback(),
                    PlaybackState::Live
                );
            }
        }
    }
    #[test]
    fn paused_recovery_cannot_commit_live_readiness() {
        let mut engine = engine();
        let old = initial(&mut engine);
        confirm_pause(&mut engine, old);
        eof(&mut engine, old);
        engine.runner.barrier(old);
        engine.poll();
        validate(&mut engine);
        let key = engine.runner.opens.last().unwrap().0;
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(receipt(settings(60))),
        });
        engine.poll();
        assert!(engine.model().active().is_none());
        assert_eq!(engine.model().phase(), ProductPhase::Recovering);
        assert_eq!(
            engine
                .model()
                .failures()
                .unwrap()
                .candidate
                .as_ref()
                .unwrap()
                .operation,
            "open_receipt"
        );
    }
    #[test]
    fn genuine_automatic_failure_remains_actionable_without_same_epoch_retry() {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        eof(&mut engine, old);
        engine.runner.barrier(old);
        engine.poll();
        validate(&mut engine);
        let failed = engine.runner.opens.last().unwrap().0;
        let failure = ApplyFailure::new(
            FailureCategory::Session,
            Stage::Negotiation,
            Cause::RequestedModeRefused,
            settings(60),
            "genuine_open",
            "mode refused",
        );
        engine.runner.events.push_back(SessionEvent::OpenFailed {
            key: failed,
            failure: failure.clone(),
        });
        engine.poll();
        engine.runner.barrier(failed.attempt);
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
        assert_eq!(engine.model().failures().unwrap().candidate, Some(failure));
        let same = engine.model().observation().unwrap().clone();
        for _ in 0..3 {
            observe(&mut engine, same.clone());
        }
        assert_eq!(engine.validator.requests.len(), 2);
        assert_eq!(engine.runner.opens.len(), 2);
        let admission = engine.reconnect(engine.model().state_identity()).unwrap();
        let ReconnectAdmission::Started(operation) = admission else {
            panic!("manual restore not admitted")
        };
        assert_eq!(
            engine.reconnect(engine.model().state_identity()),
            Ok(ReconnectAdmission::Joined(operation))
        );
        assert_eq!(
            engine.validator.request.as_ref().unwrap().settings,
            settings(60)
        );
        validate(&mut engine);
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Active);
        assert_eq!(engine.model().draft().settings, settings(30));
    }
    #[test]
    fn explicit_successful_apply_supersedes_loss_target_but_failed_apply_does_not() {
        let mut engine = engine();
        let old = initial(&mut engine);
        eof(&mut engine, old);
        let absent = observation(
            &engine,
            2,
            crate::domain::capture::VideoPresence::Absent,
            SourcePresence::Disabled,
            None,
        );
        observe(&mut engine, absent);
        engine.runner.barrier(old);
        engine.poll();
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        apply(&mut engine);
        let request = engine.validator.request.as_ref().unwrap().clone();
        engine.validator.finish(Err(failure(
            request.settings,
            FailureCategory::Validation(ValidationLayer::Mode),
            "rejected candidate",
        )));
        engine.poll();
        assert_eq!(
            engine.model().recovery().unwrap().applied.settings(),
            &settings(60)
        );
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
        apply(&mut engine);
        validate(&mut engine);
        engine.runner.verified();
        engine.poll();
        assert!(engine.model().recovery().is_none());
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(30)
        );
    }
    #[test]
    fn close_and_quit_suppress_return_and_wait_for_validation_owner_native_and_worker_retirement() {
        for quit in [false, true] {
            let mut engine = engine();
            let old = initial(&mut engine);
            eof(&mut engine, old);
            engine.runner.barrier(old);
            engine.poll();
            let request = engine.validator.request.as_ref().unwrap().clone();
            if quit {
                engine.quit();
            } else {
                engine.close(engine.model().state_identity()).unwrap();
            }
            let late = RecoveryObservation {
                stamp: request.watch,
                video: crate::domain::capture::VideoPresence::Present,
                audio: SourcePresence::Disabled,
                last_video_removal: None,
            };
            observe(&mut engine, late);
            assert_eq!(engine.model().phase(), ProductPhase::Stopping);
            assert!(engine.validator.cancelled);
            assert!(engine.validator.target.is_none());
            engine.validator.finish(Ok(()));
            engine.poll();
            assert_eq!(engine.runner.opens.len(), 1);
            if quit {
                assert!(!engine.model().shutdown_ready());
                engine.validator.retired = true;
                engine.poll();
                assert!(engine.model().shutdown_ready());
                assert_eq!(
                    engine.reconnect(engine.model().state_identity()),
                    Err(CommandRejection::ShuttingDown)
                );
            } else {
                assert_eq!(engine.model().phase(), ProductPhase::Stopped);
            }
        }
    }
    fn silent_audio_engine() -> (
        Engine,
        AttemptId,
        crate::domain::capture::AudioSourceIdentity,
    ) {
        let source =
            crate::domain::capture::AudioSourceIdentity::new("exact.capture".into(), vec![])
                .unwrap();
        let mut desired = settings(60);
        desired.audio = AudioSelection::Enabled {
            source: source.clone(),
        };
        let mut engine = ApplyCoordinator::new(
            desired.clone(),
            PlaybackGain::default(),
            Validator::default(),
            Runner::default(),
        );
        apply(&mut engine);
        let absent = observation(
            &engine,
            2,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Absent(crate::domain::capture::AudioError::Cancelled),
            None,
        );
        observe(&mut engine, absent);
        validate(&mut engine);
        let key = engine.runner.opens.last().unwrap().0;
        let mut receipt = receipt(desired);
        receipt.audio = AudioOutcome::Silent {
            source: source.clone(),
            reason: AudioSilence::WaitingForSource(crate::domain::capture::AudioError::Cancelled),
        };
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(receipt),
        });
        engine.poll();
        (engine, key.attempt, source)
    }

    #[test]
    fn matching_owner_stop_clears_audio_before_native_release_and_ignores_late_status() {
        for failed_cleanup in [false, true] {
            let (mut engine, attempt, _) = silent_audio_engine();
            let status = engine.audio_availability().unwrap().clone();
            engine.close(engine.model().state_identity()).unwrap();
            let foreign = AttemptId::new(attempt.get() + 1).unwrap();
            engine.runner.events.push_back(SessionEvent::OwnerStopped {
                attempt: foreign,
                outcome: Ok(()),
            });
            engine
                .runner
                .events
                .push_back(SessionEvent::NativeReleased { attempt: foreign });
            engine.poll();
            assert_eq!(engine.audio_availability(), Some(&status));
            let cleanup = failure(
                settings(60),
                FailureCategory::Lifecycle(LifecycleFailure::Acknowledgement),
                "owned cleanup diagnostic",
            );
            engine.runner.events.push_back(SessionEvent::OwnerStopped {
                attempt,
                outcome: if failed_cleanup {
                    Err(cleanup.clone())
                } else {
                    Ok(())
                },
            });
            engine.poll();
            assert!(engine.audio_availability().is_none());
            assert_eq!(engine.model().phase(), ProductPhase::Stopping);
            assert_eq!(engine.model().cleanup(), &CleanupStatus::Draining);
            if failed_cleanup {
                assert!(
                    engine
                        .model()
                        .failures()
                        .unwrap()
                        .cleanup
                        .contains(&cleanup)
                );
            }
            engine
                .runner
                .events
                .push_back(SessionEvent::AudioAvailability { attempt, status });
            engine.runner.events.push_back(SessionEvent::OwnerStopped {
                attempt,
                outcome: Ok(()),
            });
            engine.poll();
            assert!(engine.audio_availability().is_none());
            assert_eq!(engine.model().phase(), ProductPhase::Stopping);
            engine
                .runner
                .events
                .push_back(SessionEvent::NativeReleased { attempt });
            engine.poll();
            assert!(engine.audio_availability().is_none());
            assert_eq!(engine.model().phase(), ProductPhase::Stopped);
            assert_eq!(engine.runner.opens.len(), 1);
        }
    }
    #[test]
    fn enabled_absent_source_commits_silent_video_and_return_serializes_detach_before_reattach() {
        use crate::domain::capture::VideoPresence;
        let (mut engine, attempt, source) = silent_audio_engine();
        assert!(
            engine
                .model()
                .active()
                .unwrap()
                .applied()
                .settings()
                .audio
                .enabled()
        );
        assert!(matches!(
            engine.audio_availability(),
            Some(AudioAvailability::Silent { .. })
        ));
        let present = observation(
            &engine,
            3,
            VideoPresence::Present,
            SourcePresence::Present,
            None,
        );
        observe(&mut engine, present.clone());
        let first = AudioEpoch::new(1).unwrap();
        assert!(
            matches!(engine.runner.intents.last(), Some((current, ImmediateIntent::AttachAudio { epoch, source: selected, stamp }))
            if *current == attempt && *epoch == first && selected == &source && *stamp == present.stamp)
        );
        let route = route_fixture(attempt, first, present.stamp, source.clone());
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Active { route },
            });
        engine.poll();
        assert!(matches!(
            engine.audio_availability(),
            Some(AudioAvailability::Active { .. })
        ));
        let missing = observation(
            &engine,
            4,
            VideoPresence::Present,
            SourcePresence::Absent(crate::domain::capture::AudioError::Cancelled),
            None,
        );
        observe(&mut engine, missing);
        assert!(
            matches!(engine.audio_availability(), Some(AudioAvailability::Detaching { epoch }) if *epoch == first)
        );
        let returned = observation(
            &engine,
            5,
            VideoPresence::Present,
            SourcePresence::Present,
            None,
        );
        observe(&mut engine, returned.clone());
        assert_eq!(
            engine
                .runner
                .intents
                .iter()
                .filter(|(_, intent)| matches!(intent, ImmediateIntent::AttachAudio { .. }))
                .count(),
            1
        );
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Active {
                    route: route_fixture(attempt, first, present.stamp, source.clone()),
                },
            });
        engine.poll();
        assert!(
            matches!(engine.audio_availability(), Some(AudioAvailability::Detaching { epoch }) if *epoch == first)
        );
        engine.runner.events.push_back(SessionEvent::AudioDetached {
            attempt,
            epoch: first,
            outcome: Ok(()),
        });
        engine.poll();
        let second = AudioEpoch::new(2).unwrap();
        assert!(
            matches!(engine.audio_availability(), Some(AudioAvailability::Opening { epoch }) if *epoch == second)
        );
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
        engine.runner.events.push_back(SessionEvent::AudioDetached {
            attempt,
            epoch: first,
            outcome: Ok(()),
        });
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Active {
                    route: route_fixture(attempt, first, present.stamp, source.clone()),
                },
            });
        engine.poll();
        assert!(
            matches!(engine.audio_availability(), Some(AudioAvailability::Opening { epoch }) if *epoch == second)
        );
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Active {
                    route: route_fixture(attempt, second, returned.stamp, source.clone()),
                },
            });
        engine.poll();
        assert!(
            matches!(engine.audio_availability(), Some(AudioAvailability::Active { route, .. }) if route.epoch() == second && route.source() == &source)
        );
        assert_eq!(engine.model().active().unwrap().attempt(), attempt);
        assert_eq!(
            engine
                .runner
                .intents
                .iter()
                .filter(|(_, intent)| matches!(intent, ImmediateIntent::AttachAudio { .. }))
                .count(),
            2
        );
    }
    #[test]
    fn pending_wrong_source_wrong_stamp_and_wrong_attempt_never_become_active() {
        use crate::domain::capture::{AudioSourceIdentity, VideoPresence};
        for wrong in 0..4 {
            let (mut engine, attempt, source) = silent_audio_engine();
            let present = observation(
                &engine,
                3,
                VideoPresence::Present,
                SourcePresence::Present,
                None,
            );
            observe(&mut engine, present.clone());
            let epoch = AudioEpoch::new(1).unwrap();
            engine
                .runner
                .events
                .push_back(SessionEvent::AudioAvailability {
                    attempt,
                    status: AudioAvailability::Silent {
                        reason: AudioSilence::PendingRoute,
                    },
                });
            engine.poll();
            assert!(!matches!(
                engine.audio_availability(),
                Some(AudioAvailability::Active { .. })
            ));
            let mut stamp = present.stamp;
            let route_attempt = if wrong == 0 {
                AttemptId::new(attempt.get() + 1).unwrap()
            } else {
                attempt
            };
            let route_epoch = if wrong == 3 {
                AudioEpoch::new(2).unwrap()
            } else {
                epoch
            };
            if wrong == 2 {
                stamp.epoch = crate::domain::capture::ObservationEpoch::new(2).unwrap();
            }
            let selected = if wrong == 1 {
                AudioSourceIdentity::new("microphone".into(), vec![]).unwrap()
            } else {
                source.clone()
            };
            let route = route_fixture(route_attempt, route_epoch, stamp, selected);
            engine
                .runner
                .events
                .push_back(SessionEvent::AudioAvailability {
                    attempt,
                    status: AudioAvailability::Active { route },
                });
            engine.poll();
            assert!(!matches!(
                engine.audio_availability(),
                Some(AudioAvailability::Active { .. })
            ));
            assert_eq!(engine.model().active().unwrap().attempt(), attempt);
            assert!(engine.runner.stops.is_empty());
        }
    }
    #[test]
    fn unsafe_audio_detach_failure_is_not_a_synthetic_retirement_or_new_attachment() {
        let (mut engine, attempt, _) = silent_audio_engine();
        let present = observation(
            &engine,
            3,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Present,
            None,
        );
        observe(&mut engine, present);
        engine.runner.events.push_back(SessionEvent::AudioDetached {
            attempt,
            epoch: AudioEpoch::new(1).unwrap(),
            outcome: Err(crate::domain::capture::AudioError::Cancelled),
        });
        engine.poll();
        assert!(engine.model().active().is_none());
        assert_eq!(engine.runner.stops.len(), 1);
        assert_eq!(engine.runner.opens.len(), 1);
        assert_eq!(
            engine
                .model()
                .failures()
                .unwrap()
                .incumbent
                .as_ref()
                .unwrap()
                .operation,
            "audio_detach"
        );
    }
    #[test]
    fn explicit_target_cutover_waits_for_actual_retirement_and_fresh_full_validation() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let original_watch = engine.model().watch_target().unwrap().watch;
        engine.validator.defer_cutover = true;
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        apply(&mut engine);
        validate(&mut engine);
        assert_eq!(engine.model().watch_target().unwrap().watch, original_watch);
        assert_eq!(engine.runner.opens.len(), 1);
        engine.runner.events.push_back(SessionEvent::OwnerStopped {
            attempt: old,
            outcome: Ok(()),
        });
        engine.poll();
        assert_eq!(engine.model().watch_target().unwrap().watch, original_watch);
        engine
            .runner
            .events
            .push_back(SessionEvent::NativeReleased { attempt: old });
        engine.poll();
        assert_ne!(engine.model().watch_target().unwrap().watch, original_watch);
        assert_eq!(engine.runner.opens.len(), 1);
        let request = engine.validator.request.as_ref().unwrap().clone();
        assert_eq!(request.settings, settings(30));
        assert_eq!(
            request.watch.watch,
            engine.model().watch_target().unwrap().watch
        );
        assert_eq!(engine.validator.cutovers.len(), 1);
        engine.validator.finish(Err(failure(
            settings(30),
            FailureCategory::Validation(ValidationLayer::Mode),
            "fresh cutover tuple rejected",
        )));
        engine.poll();
        assert_eq!(engine.runner.opens.len(), 1);
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
    }
    #[test]
    fn no_history_reconnect_is_actionable_and_quit_requires_both_worker_joins() {
        let mut engine = engine();
        assert_eq!(
            engine.reconnect(engine.model().state_identity()),
            Err(CommandRejection::ReconnectUnavailable)
        );
        engine.validator.watcher_join_pending = true;
        engine.quit();
        engine.validator.retired = true;
        engine.poll();
        assert!(!engine.model().shutdown_ready());
        engine.validator.watcher_join_pending = false;
        engine.poll();
        assert!(engine.model().shutdown_ready());
    }
    #[test]
    fn audio_only_loss_during_initial_readiness_keeps_video_and_waits_for_real_detachment() {
        use crate::domain::capture::{AudioSourceIdentity, VideoPresence};
        let source = AudioSourceIdentity::new("capture".into(), vec![]).unwrap();
        let mut desired = settings(60);
        desired.audio = AudioSelection::Enabled {
            source: source.clone(),
        };
        let mut engine = ApplyCoordinator::new(
            desired.clone(),
            PlaybackGain::default(),
            Validator::default(),
            Runner::default(),
        );
        apply(&mut engine);
        validate(&mut engine);
        let key = engine.runner.opens.last().unwrap().0;
        let epoch = AudioEpoch::new(1).unwrap();
        let route = route_fixture(
            key.attempt,
            epoch,
            engine.lease.as_ref().unwrap().watch,
            source.clone(),
        );
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt: key.attempt,
                status: AudioAvailability::Active {
                    route: route.clone(),
                },
            });
        let mut ready = receipt(desired);
        ready.audio = AudioOutcome::Active { route };
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(ready),
        });
        let absent = observation(
            &engine,
            2,
            VideoPresence::Present,
            SourcePresence::Absent(crate::domain::capture::AudioError::Cancelled),
            None,
        );
        observe(&mut engine, absent);
        assert!(engine.model().active().is_none());
        assert!(engine.runner.stops.is_empty());
        assert!(engine.deferred_ready.is_some());
        engine.runner.events.push_back(SessionEvent::AudioDetached {
            attempt: key.attempt,
            epoch,
            outcome: Ok(()),
        });
        engine.poll();
        assert_eq!(engine.model().active().unwrap().attempt(), key.attempt);
        assert!(matches!(
            engine.audio_availability(),
            Some(AudioAvailability::Silent { .. })
        ));
        assert!(engine.runner.stops.is_empty());
        assert_eq!(engine.runner.opens.len(), 1);
    }
    #[test]
    fn completed_validation_older_than_latest_observation_revalidates_without_opening_twice() {
        let mut engine = engine();
        apply(&mut engine);
        let stale = engine.validator.request.take().unwrap();
        let newer = observation(
            &engine,
            2,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Disabled,
            None,
        );
        observe(&mut engine, newer);
        engine.validator.result = Some(ValidationResult {
            stamp: stale.watch,
            result: ValidationOutcome::Prepared(TestPrepared {
                settings: stale.settings.clone(),
                stamp: stale.watch,
            }),
            request: stale,
        });
        engine.poll();
        assert!(engine.runner.opens.is_empty());
        assert_eq!(
            engine.validator.request.as_ref().unwrap().watch.epoch.get(),
            2
        );
        validate(&mut engine);
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Active);
        assert_eq!(engine.runner.opens.len(), 1);
    }
    #[test]
    fn fresh_validation_scan_does_not_turn_real_automatic_failure_into_retry_loop() {
        for fail_validation in [false, true] {
            let mut engine = engine();
            let old = initial(&mut engine);
            eof(&mut engine, old);
            engine.runner.barrier(old);
            engine.poll();
            let fresh = observation(
                &engine,
                2,
                crate::domain::capture::VideoPresence::Present,
                SourcePresence::Disabled,
                None,
            );
            observe(&mut engine, fresh.clone());
            let actual = failure(
                settings(60),
                FailureCategory::Session,
                "persistent actual failure",
            );
            if fail_validation {
                engine.validator.finish(Err(actual.clone()));
                engine.poll();
            } else {
                validate(&mut engine);
                let key = engine.runner.opens.last().unwrap().0;
                engine.runner.events.push_back(SessionEvent::OpenFailed {
                    key,
                    failure: actual.clone(),
                });
                engine.poll();
                engine.runner.barrier(key.attempt);
                engine.poll();
            }
            assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
            assert_eq!(engine.model().failures().unwrap().candidate, Some(actual));
            let requests = engine.validator.requests.len();
            let opens = engine.runner.opens.len();
            for _ in 0..3 {
                observe(&mut engine, fresh.clone());
            }
            assert_eq!(engine.validator.requests.len(), requests);
            assert_eq!(engine.runner.opens.len(), opens);
            assert!(engine.validator.request.is_none());
            assert!(matches!(
                engine.reconnect(engine.model().state_identity()),
                Ok(ReconnectAdmission::Started(_))
            ));
        }
    }
    #[test]
    fn coalesced_initial_detached_receipt_consumes_epoch_without_prior_opening_snapshot() {
        let source =
            crate::domain::capture::AudioSourceIdentity::new("exact.capture".into(), vec![])
                .unwrap();
        let mut desired = settings(60);
        desired.audio = AudioSelection::Enabled {
            source: source.clone(),
        };
        let mut engine = ApplyCoordinator::new(
            desired.clone(),
            PlaybackGain::default(),
            Validator::default(),
            Runner::default(),
        );
        apply(&mut engine);
        validate(&mut engine);
        let key = engine.runner.opens.last().unwrap().0;
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt: key.attempt,
                status: AudioAvailability::Silent {
                    reason: AudioSilence::WaitingForSource(
                        crate::domain::capture::AudioError::Cancelled,
                    ),
                },
            });
        engine.runner.events.push_back(SessionEvent::AudioDetached {
            attempt: key.attempt,
            epoch: AudioEpoch::new(1).unwrap(),
            outcome: Ok(()),
        });
        let mut ready = receipt(desired);
        ready.audio = AudioOutcome::Silent {
            source: source.clone(),
            reason: AudioSilence::WaitingForSource(crate::domain::capture::AudioError::Cancelled),
        };
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(ready),
        });
        engine.poll();
        assert_eq!(engine.model().active().unwrap().attempt(), key.attempt);
        assert!(engine.runner.intents.is_empty());
        let returned = observation(
            &engine,
            2,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Present,
            None,
        );
        observe(&mut engine, returned.clone());
        assert!(
            matches!(engine.runner.intents.last(), Some((attempt, ImmediateIntent::AttachAudio { epoch, source: selected, stamp }))
            if *attempt == key.attempt && epoch.get() == 2 && selected == &source && *stamp == returned.stamp)
        );
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
    }
    fn opening_choice(engine: &mut Engine, choose_other: bool) -> (AttemptKey, SelectionToken) {
        use crate::domain::capture::{CandidateId, RecoveryCandidate, VideoPresence};
        let old = initial(engine);
        eof(engine, old);
        let other = DeviceIdentity::new(
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
        let mut ambiguous = observation(
            engine,
            2,
            VideoPresence::Absent,
            SourcePresence::Disabled,
            None,
        );
        let saved = SelectionToken {
            stamp: ambiguous.stamp,
            candidate: CandidateId::new(1).unwrap(),
        };
        let different = SelectionToken {
            stamp: ambiguous.stamp,
            candidate: CandidateId::new(2).unwrap(),
        };
        ambiguous.video = VideoPresence::Ambiguous(vec![
            RecoveryCandidate {
                token: saved,
                identity: settings(60).video.identity,
                description: "saved A".into(),
            },
            RecoveryCandidate {
                token: different,
                identity: other.clone(),
                description: "selected B".into(),
            },
        ]);
        observe(engine, ambiguous);
        engine.runner.barrier(old);
        engine.poll();
        let token = if choose_other { different } else { saved };
        engine
            .choose_recovery(engine.model().state_identity(), token)
            .unwrap();
        let request = engine.validator.request.take().unwrap();
        let mut selected = request.settings.clone();
        if choose_other {
            selected.video.identity = other;
        }
        engine.validator.result = Some(ValidationResult {
            stamp: request.watch,
            result: ValidationOutcome::Prepared(TestPrepared {
                settings: selected,
                stamp: request.watch,
            }),
            request,
        });
        engine.poll();
        (engine.runner.opens.last().unwrap().0, token)
    }
    #[test]
    fn failed_chosen_open_retires_grant_only_at_real_barrier_and_restores_returned_saved_identity()
    {
        let mut engine = engine();
        let (chosen, token) = opening_choice(&mut engine, true);
        assert_eq!(engine.validator.grant, Some(token));
        engine.runner.fail("genuine selected B open failure");
        engine.poll();
        assert_eq!(engine.validator.grant, Some(token));
        let returned = observation(
            &engine,
            3,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Disabled,
            None,
        );
        observe(&mut engine, returned);
        engine
            .runner
            .events
            .push_back(SessionEvent::NativeReleased {
                attempt: chosen.attempt,
            });
        engine.poll();
        assert_eq!(engine.validator.grant, Some(token));
        assert_eq!(engine.runner.opens.len(), 2);
        engine.runner.events.push_back(SessionEvent::OwnerStopped {
            attempt: chosen.attempt,
            outcome: Ok(()),
        });
        engine.poll();
        assert_eq!(engine.validator.grant, Some(token));
        engine
            .runner
            .events
            .push_back(SessionEvent::NativeReleased {
                attempt: chosen.attempt,
            });
        engine.poll();
        assert!(engine.validator.grant.is_none());
        assert_eq!(engine.validator.retired_choices, vec![token]);
        let automatic = engine.validator.request.as_ref().unwrap();
        assert_eq!(automatic.settings, settings(60));
        assert!(automatic.choice.is_none());
        validate(&mut engine);
        engine.runner.verified();
        engine.poll();
        assert_eq!(
            engine.model().active().unwrap().applied().settings(),
            &settings(60)
        );
        assert_eq!(engine.model().draft().settings, settings(60));
        assert_eq!(engine.runner.opens.len(), 3);
    }
    #[test]
    fn verified_choice_equal_to_saved_tuple_retires_ephemeral_grant_without_watch_replacement() {
        let mut engine = engine();
        let (chosen, token) = opening_choice(&mut engine, false);
        let watch = engine.model().watch_target().unwrap().watch;
        assert_eq!(engine.validator.grant, Some(token));
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.model().active().unwrap().attempt(), chosen.attempt);
        assert_eq!(engine.model().watch_target().unwrap().watch, watch);
        assert!(engine.validator.grant.is_none());
        assert_eq!(engine.validator.retired_choices, vec![token]);
        eof(&mut engine, chosen.attempt);
        let returned = observation(
            &engine,
            3,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Disabled,
            None,
        );
        observe(&mut engine, returned);
        let resumed = recovered(&mut engine, chosen.attempt);
        assert_ne!(resumed, chosen.attempt);
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
        assert!(engine.validator.requests.last().unwrap().choice.is_none());
        assert_eq!(engine.runner.opens.len(), 3);
    }
    fn output_silent_engine() -> (
        Engine,
        AttemptId,
        crate::domain::capture::AudioSourceIdentity,
    ) {
        let source =
            crate::domain::capture::AudioSourceIdentity::new("exact.capture".into(), vec![])
                .unwrap();
        let mut desired = settings(60);
        desired.audio = AudioSelection::Enabled {
            source: source.clone(),
        };
        let mut engine = ApplyCoordinator::new(
            desired.clone(),
            PlaybackGain::default(),
            Validator::default(),
            Runner::default(),
        );
        engine
            .set_output_plan(OutputPlan::Silent {
                revision: OutputRevision::first(),
                reason: OutputSilence::ManualRequiresAction,
            })
            .unwrap();
        let apply = apply(&mut engine);
        engine.require_startup_restore_audio(apply).unwrap();
        validate(&mut engine);
        let key = engine.runner.opens.last().unwrap().0;
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt: key.attempt,
                status: AudioAvailability::Silent {
                    reason: AudioSilence::Output(OutputSilence::ManualRequiresAction),
                },
            });
        let mut opened = receipt(desired);
        opened.audio = AudioOutcome::Silent {
            source: source.clone(),
            reason: AudioSilence::Output(OutputSilence::ManualRequiresAction),
        };
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(opened),
        });
        engine.poll();
        assert_eq!(engine.model().active().unwrap().attempt(), key.attempt);
        assert!(engine.lease.as_ref().unwrap().audio_retired);
        assert!(engine.lease.as_ref().unwrap().audio_epoch.is_none());
        assert!(engine.runner.intents.is_empty());
        (engine, key.attempt, source)
    }

    #[test]
    fn initial_output_silence_verifies_video_and_target_return_admits_only_fresh_audio_epoch() {
        let (mut engine, attempt, source) = output_silent_engine();
        let applied = engine.model().last_valid().unwrap().settings().clone();
        let mut dirty = applied.clone();
        dirty.video.mode.rate = FrameRate::new(30, 1).unwrap();
        engine
            .edit_draft(engine.model().draft().revision, dirty.clone())
            .unwrap();
        let revision = engine.output.revision().next();
        let target = OutputPlan::Target {
            revision,
            target: output_fixture(),
        };
        assert_eq!(
            engine.set_output_plan(target.clone()),
            Ok(SubmitStatus::Accepted)
        );
        assert_eq!(engine.runner.intents.len(), 2);
        assert_eq!(
            engine.runner.intents[0],
            (attempt, ImmediateIntent::SetOutput(target))
        );
        let epoch = AudioEpoch::new(1).unwrap();
        assert!(
            matches!(&engine.runner.intents[1], (active, ImmediateIntent::AttachAudio { epoch: admitted, source: selected, stamp })
            if *active == attempt && *admitted == epoch && selected == &source && *stamp == engine.lease.as_ref().unwrap().watch)
        );
        let route = crate::media::loopback::LoopbackReceipt::for_test(
            crate::media::controller::Generation::new(attempt.get()).unwrap(),
            attempt,
            epoch,
            engine.lease.as_ref().unwrap().watch,
            source.clone(),
            output_fixture(),
            revision,
        );
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Active { route },
            });
        engine.poll();
        assert!(matches!(
            engine.audio_availability(),
            Some(AudioAvailability::Active { .. })
        ));
        assert_eq!(engine.model().active().unwrap().attempt(), attempt);
        assert_eq!(engine.model().last_valid().unwrap().settings(), &applied);
        assert_eq!(engine.model().draft().settings, dirty);
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
    }

    #[test]
    fn terminal_audio_failure_is_not_no_owner_output_availability_or_source_recovery() {
        let (mut engine, attempt, _) = output_silent_engine();
        let revision = engine.output.revision().next();
        engine
            .set_output_plan(OutputPlan::Target {
                revision,
                target: output_fixture(),
            })
            .unwrap();
        let epoch = AudioEpoch::new(1).unwrap();
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Silent {
                    reason: AudioSilence::Failed(crate::domain::capture::AudioError::Control(
                        "helper died".into(),
                    )),
                },
            });
        engine.runner.events.push_back(SessionEvent::AudioDetached {
            attempt,
            epoch,
            outcome: Ok(()),
        });
        engine.poll();
        engine
            .set_output_plan(OutputPlan::Target {
                revision: revision.next(),
                target: output_fixture(),
            })
            .unwrap();
        let present = observation(
            &engine,
            3,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Present,
            None,
        );
        observe(&mut engine, present);
        assert!(engine.lease.as_ref().unwrap().audio_terminal);
        assert!(engine.lease.as_ref().unwrap().audio_retired);
        assert!(matches!(
            engine.audio_availability(),
            Some(AudioAvailability::Silent {
                reason: AudioSilence::Failed(_)
            })
        ));
        assert_eq!(
            engine
                .runner
                .intents
                .iter()
                .filter(|(_, intent)| matches!(intent, ImmediateIntent::AttachAudio { .. }))
                .count(),
            1
        );
        assert_eq!(engine.model().active().unwrap().attempt(), attempt);
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
    }

    #[test]
    fn paused_output_and_manual_wait_latch_survive_fresh_applied_resume_not_dirty_draft() {
        let (mut engine, attempt, _) = output_silent_engine();
        engine.pause(attempt).unwrap();
        let request = match &engine.runner.intents.last().unwrap().1 {
            ImmediateIntent::SetPaused { request, .. } => *request,
            _ => unreachable!(),
        };
        engine.handle_event(SessionEvent::PauseObserved {
            attempt,
            request,
            paused: true,
        });
        let applied = engine.model().last_valid().unwrap().settings().clone();
        let mut dirty = applied.clone();
        dirty.video.mode.rate = FrameRate::new(30, 1).unwrap();
        engine
            .edit_draft(engine.model().draft().revision, dirty.clone())
            .unwrap();
        let waiting = OutputPlan::Silent {
            revision: engine.output.revision().next(),
            reason: OutputSilence::ManualRequiresAction,
        };
        engine.set_output_plan(waiting.clone()).unwrap();
        assert!(
            !engine
                .runner
                .intents
                .iter()
                .any(|(_, intent)| matches!(intent, ImmediateIntent::AttachAudio { .. }))
        );
        engine
            .resume(engine.model().state_identity(), attempt)
            .unwrap();
        engine.runner.barrier(attempt);
        engine.poll();
        validate(&mut engine);
        assert_eq!(engine.runner.opens.len(), 2);
        assert_eq!(engine.runner.opens.last().unwrap().1, applied);
        assert_eq!(engine.runner.outputs.last(), Some(&waiting));
        assert_eq!(engine.output_plan(), &waiting);
        assert_eq!(engine.model().draft().settings, dirty);
    }

    #[test]
    fn stale_or_conflicting_output_revision_never_changes_draft_applied_or_current_intent() {
        let (mut engine, attempt, _) = output_silent_engine();
        let before_draft = engine.model().draft().clone();
        let before_applied = engine.model().last_valid().unwrap().clone();
        let revision = engine.output.revision().next();
        let current = OutputPlan::Target {
            revision,
            target: output_fixture(),
        };
        engine.set_output_plan(current.clone()).unwrap();
        let intents = engine.runner.intents.len();
        assert_eq!(
            engine.set_output_plan(OutputPlan::Silent {
                revision: OutputRevision::first(),
                reason: OutputSilence::NoAvailableOutput
            }),
            Err(CommandRejection::StaleState)
        );
        assert_eq!(
            engine.set_output_plan(OutputPlan::Silent {
                revision,
                reason: OutputSilence::ManualUnavailable
            }),
            Err(CommandRejection::StaleState)
        );
        assert_eq!(engine.output_plan(), &current);
        assert_eq!(engine.runner.intents.len(), intents);
        assert_eq!(engine.model().draft(), &before_draft);
        assert_eq!(engine.model().last_valid(), Some(&before_applied));
        assert_eq!(engine.model().active().unwrap().attempt(), attempt);
    }

    #[test]
    fn live_output_silence_and_return_only_move_playback_keep_epoch_and_ignore_old_active() {
        let (mut engine, attempt, source) = output_silent_engine();
        let first = engine.output.revision().next();
        engine
            .set_output_plan(OutputPlan::Target {
                revision: first,
                target: output_fixture(),
            })
            .unwrap();
        let epoch = AudioEpoch::new(1).unwrap();
        let route = crate::media::loopback::LoopbackReceipt::for_test(
            crate::media::controller::Generation::new(attempt.get()).unwrap(),
            attempt,
            epoch,
            engine.lease.as_ref().unwrap().watch,
            source.clone(),
            output_fixture(),
            first,
        );
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Active {
                    route: route.clone(),
                },
            });
        engine.poll();
        let silent = first.next();
        engine
            .set_output_plan(OutputPlan::Silent {
                revision: silent,
                reason: OutputSilence::ManualRequiresAction,
            })
            .unwrap();
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Silent {
                    reason: AudioSilence::Output(OutputSilence::ManualRequiresAction),
                },
            });
        engine.poll();
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Active {
                    route: route.clone(),
                },
            });
        engine.poll();
        assert!(matches!(
            engine.audio_availability(),
            Some(AudioAvailability::Silent {
                reason: AudioSilence::Output(OutputSilence::ManualRequiresAction)
            })
        ));
        engine
            .set_output_plan(OutputPlan::Target {
                revision: silent.next(),
                target: output_fixture(),
            })
            .unwrap();
        let current = crate::media::loopback::LoopbackReceipt::for_test(
            crate::media::controller::Generation::new(attempt.get()).unwrap(),
            attempt,
            epoch,
            engine.lease.as_ref().unwrap().watch,
            source,
            output_fixture(),
            silent.next(),
        );
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Active {
                    route: current.clone(),
                },
            });
        engine.poll();
        assert_eq!(
            engine.audio_availability(),
            Some(&AudioAvailability::Active {
                route: current.clone()
            })
        );
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Active { route },
            });
        engine.poll();
        assert_eq!(
            engine.audio_availability(),
            Some(&AudioAvailability::Active { route: current })
        );
        assert_eq!(engine.lease.as_ref().unwrap().audio_epoch, Some(epoch));
        assert!(!engine.lease.as_ref().unwrap().audio_retired);
        assert_eq!(
            engine
                .runner
                .intents
                .iter()
                .filter(|(_, intent)| matches!(intent, ImmediateIntent::AttachAudio { .. }))
                .count(),
            1
        );
        assert!(
            !engine
                .runner
                .intents
                .iter()
                .any(|(_, intent)| matches!(intent, ImmediateIntent::DetachAudio { .. }))
        );
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
    }

    #[test]
    fn unavailable_output_cannot_hide_missing_requested_source_from_startup_restore() {
        let source =
            crate::domain::capture::AudioSourceIdentity::new("absent.capture".into(), vec![])
                .unwrap();
        let mut desired = settings(60);
        desired.audio = AudioSelection::Enabled {
            source: source.clone(),
        };
        let mut engine = ApplyCoordinator::new(
            desired.clone(),
            PlaybackGain::default(),
            Validator::default(),
            Runner::default(),
        );
        engine
            .set_output_plan(OutputPlan::Silent {
                revision: OutputRevision::first(),
                reason: OutputSilence::NoAvailableOutput,
            })
            .unwrap();
        let apply = apply(&mut engine);
        engine.require_startup_restore_audio(apply).unwrap();
        let absent = observation(
            &engine,
            2,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Absent(crate::domain::capture::AudioError::SourceMissing {
                name: source.name().into(),
            }),
            None,
        );
        observe(&mut engine, absent);
        validate(&mut engine);
        let key = engine.runner.opens.last().unwrap().0;
        let mut opened = receipt(desired);
        opened.audio = AudioOutcome::Silent {
            source,
            reason: AudioSilence::Output(OutputSilence::NoAvailableOutput),
        };
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(opened),
        });
        engine.poll();
        assert!(engine.model().active().is_none());
        assert!(engine.model().last_valid().is_none());
        assert_eq!(
            engine.runner.stops.last(),
            Some(&(key.attempt, StopReason::Failed))
        );
        assert!(engine.take_verified_open().is_none());
    }

    #[test]
    fn rejected_output_intent_does_not_commit_revision_or_implicitly_retry_audio() {
        for rejection in [
            SubmitStatus::StaleGeneration,
            SubmitStatus::NotReady,
            SubmitStatus::Closing,
            SubmitStatus::CapacityExceeded,
        ] {
            let (mut engine, attempt, _) = output_silent_engine();
            let before = engine.output.clone();
            let revision = before.revision().next();
            let candidate = OutputPlan::Target {
                revision,
                target: output_fixture(),
            };
            engine.runner.immediate = Some(rejection);
            assert_eq!(engine.set_output_plan(candidate), Ok(rejection));
            assert_eq!(engine.output_plan(), &before);
            engine.poll();
            assert!(
                !engine
                    .runner
                    .intents
                    .iter()
                    .any(|(_, intent)| matches!(intent, ImmediateIntent::AttachAudio { .. }))
            );
            assert_eq!(engine.runner.opens.len(), 1);
            assert_eq!(engine.model().active().unwrap().attempt(), attempt);
            // The same unconsumed revision is usable by a later explicit action.
            assert_eq!(
                engine.set_output_plan(OutputPlan::Silent {
                    revision,
                    reason: OutputSilence::ManualUnavailable
                }),
                Ok(SubmitStatus::Accepted)
            );
            assert_eq!(engine.output.revision(), revision);
        }
    }

    #[test]
    fn idle_output_is_accepted_but_closing_output_rejects_without_owner_or_plan_side_effects() {
        let mut idle = engine();
        let candidate = OutputPlan::Target {
            revision: OutputRevision::first(),
            target: output_fixture(),
        };
        assert_eq!(
            idle.set_output_plan(candidate.clone()),
            Ok(SubmitStatus::Accepted)
        );
        assert_eq!(idle.output_plan(), &candidate);
        assert!(idle.runner.intents.is_empty() && idle.runner.opens.is_empty());
        let (mut closing, attempt, _) = output_silent_engine();
        let before = closing.output.clone();
        closing.close(closing.model().state_identity()).unwrap();
        let intents = closing.runner.intents.len();
        assert_eq!(
            closing.set_output_plan(OutputPlan::Target {
                revision: before.revision().next(),
                target: output_fixture()
            }),
            Ok(SubmitStatus::Closing)
        );
        assert_eq!(closing.output_plan(), &before);
        assert_eq!(closing.runner.intents.len(), intents);
        assert_eq!(
            closing.runner.stops.last(),
            Some(&(attempt, StopReason::Close))
        );
    }

    #[test]
    fn superseded_no_resource_epoch_retirement_allows_later_target_without_video_restart() {
        let (mut engine, attempt, source) = output_silent_engine();
        let first_revision = engine.output.revision().next();
        engine
            .set_output_plan(OutputPlan::Target {
                revision: first_revision,
                target: output_fixture(),
            })
            .unwrap();
        let first = AudioEpoch::new(1).unwrap();
        assert_eq!(engine.lease.as_ref().unwrap().audio_epoch, Some(first));
        let silent_revision = first_revision.next();
        engine
            .set_output_plan(OutputPlan::Silent {
                revision: silent_revision,
                reason: OutputSilence::ManualRequiresAction,
            })
            .unwrap();
        // Actual owner/gate completion for the admitted epoch that spawned nothing.
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Silent {
                    reason: AudioSilence::Output(OutputSilence::ManualRequiresAction),
                },
            });
        engine.runner.events.push_back(SessionEvent::AudioDetached {
            attempt,
            epoch: first,
            outcome: Ok(()),
        });
        engine.poll();
        assert!(engine.lease.as_ref().unwrap().audio_retired);
        assert!(engine.lease.as_ref().unwrap().audio_epoch.is_none());
        engine
            .set_output_plan(OutputPlan::Target {
                revision: silent_revision.next(),
                target: output_fixture(),
            })
            .unwrap();
        let second = AudioEpoch::new(2).unwrap();
        assert_eq!(engine.lease.as_ref().unwrap().audio_epoch, Some(second));
        assert!(
            matches!(engine.runner.intents.last(), Some((active, ImmediateIntent::AttachAudio { epoch, source: selected, .. }))
            if *active == attempt && *epoch == second && selected == &source)
        );
        assert_eq!(engine.runner.opens.len(), 1);
        assert_eq!(engine.model().active().unwrap().attempt(), attempt);
        assert!(engine.runner.stops.is_empty());
    }

    #[test]
    fn source_retirement_gain_and_join_preserve_fresh_same_source_recovery_and_video() {
        let (mut engine, attempt, source) = silent_audio_engine();
        let present = observation(
            &engine,
            3,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Present,
            None,
        );
        observe(&mut engine, present.clone());
        let first = AudioEpoch::new(1).unwrap();
        engine
            .runner
            .events
            .push_back(SessionEvent::AudioAvailability {
                attempt,
                status: AudioAvailability::Active {
                    route: route_fixture(attempt, first, present.stamp, source.clone()),
                },
            });
        engine.poll();
        let lost = observation(
            &engine,
            4,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Absent(crate::domain::capture::AudioError::SourceMissing {
                name: source.name().into(),
            }),
            None,
        );
        observe(&mut engine, lost);
        let gain = PlaybackGain::new(37, true).unwrap();
        assert_eq!(engine.set_gain(gain), SubmitStatus::Accepted);
        assert_eq!(engine.gain(), gain);
        engine.runner.events.push_back(SessionEvent::AudioDetached {
            attempt,
            epoch: first,
            outcome: Ok(()),
        });
        engine.poll();
        let returned = observation(
            &engine,
            5,
            crate::domain::capture::VideoPresence::Present,
            SourcePresence::Present,
            None,
        );
        observe(&mut engine, returned);
        assert!(!engine.lease.as_ref().unwrap().audio_terminal);
        assert_eq!(
            engine.lease.as_ref().unwrap().audio_epoch,
            Some(AudioEpoch::new(2).unwrap())
        );
        assert!(
            matches!(engine.runner.intents.last(), Some((active, ImmediateIntent::AttachAudio { epoch, source: selected, .. }))
            if *active == attempt && epoch.get() == 2 && selected == &source)
        );
        assert_eq!(engine.gain(), gain);
        assert_eq!(engine.runner.opens.len(), 1);
        assert_eq!(engine.model().active().unwrap().attempt(), attempt);
        assert!(engine.runner.stops.is_empty());
    }

    #[test]
    fn terminal_failure_after_pending_source_or_pause_retirement_cannot_auto_retry() {
        for paused in [false, true] {
            let (mut engine, attempt, _) = output_silent_engine();
            let revision = engine.output.revision().next();
            engine
                .set_output_plan(OutputPlan::Target {
                    revision,
                    target: output_fixture(),
                })
                .unwrap();
            let epoch = AudioEpoch::new(1).unwrap();
            let reason = if paused {
                AudioSilence::Paused
            } else {
                AudioSilence::WaitingForSource(crate::domain::capture::AudioError::SourceMissing {
                    name: "exact.capture".into(),
                })
            };
            engine
                .runner
                .events
                .push_back(SessionEvent::AudioAvailability {
                    attempt,
                    status: AudioAvailability::Detaching { epoch },
                });
            engine
                .runner
                .events
                .push_back(SessionEvent::AudioAvailability {
                    attempt,
                    status: AudioAvailability::Silent { reason },
                });
            engine.poll();
            let failure = crate::domain::capture::AudioError::Control("bounded reap failed".into());
            engine
                .runner
                .events
                .push_back(SessionEvent::AudioAvailability {
                    attempt,
                    status: AudioAvailability::Silent {
                        reason: AudioSilence::Failed(failure.clone()),
                    },
                });
            engine.runner.events.push_back(SessionEvent::AudioDetached {
                attempt,
                epoch,
                outcome: Ok(()),
            });
            engine.poll();
            engine
                .set_output_plan(OutputPlan::Target {
                    revision: revision.next(),
                    target: output_fixture(),
                })
                .unwrap();
            let returned = observation(
                &engine,
                3,
                crate::domain::capture::VideoPresence::Present,
                SourcePresence::Present,
                None,
            );
            observe(&mut engine, returned);
            assert!(
                engine.lease.as_ref().unwrap().audio_terminal
                    && engine.lease.as_ref().unwrap().audio_retired
            );
            assert_eq!(
                engine.audio_availability(),
                Some(&AudioAvailability::Silent {
                    reason: AudioSilence::Failed(failure)
                })
            );
            assert_eq!(
                engine
                    .runner
                    .intents
                    .iter()
                    .filter(|(_, intent)| matches!(intent, ImmediateIntent::AttachAudio { .. }))
                    .count(),
                1
            );
            assert_eq!(engine.runner.opens.len(), 1);
            assert_eq!(engine.model().active().unwrap().attempt(), attempt);
            assert!(engine.runner.stops.is_empty());
        }
    }
}
