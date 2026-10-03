//! Serialized product orchestration over opaque validation and session ports.

use crate::domain::{
    capture::PlaybackGain,
    failure::{ApplyFailure, Cause, FailureCategory, LifecycleFailure, Stage, ValidationLayer},
    state::{
        ApplyId, AttemptId, AttemptKey, CommandRejection, DraftRevision, DraftSettings,
        ModelEffect, ProductModel, StateIdentity, StopIntent, ValidationRequest,
    },
};

use super::ports::{
    AudioDiagnostic, DraftValidator, ImmediateIntent, OpenReceipt, SessionEvent, SessionRunner,
    StartFailure, StopReason, StopSubmission, SubmitFailure, SubmitStatus,
};

struct Lease {
    key: AttemptKey,
    settings: DraftSettings,
    owner_stopped: bool,
    stopping: bool,
    verified: bool,
    blocked: bool,
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
    validation: Option<ValidationRequest>,
    prepared: Option<(ValidationRequest, V::Prepared)>,
    lease: Option<Lease>,
    audio: Option<AudioDiagnostic>,
    validator_shutdown: bool,
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
            validation: None,
            prepared: None,
            lease: None,
            audio: None,
            validator_shutdown: false,
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
    pub fn audio_diagnostic(&self) -> Option<&AudioDiagnostic> {
        self.audio.as_ref()
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
    ) -> Result<ApplyId, CommandRejection> {
        self.check_drain()?;
        let (id, effect) = self.model.reconnect(expected_state)?;
        self.drive(Some(effect))?;
        Ok(id)
    }
    pub fn close(&mut self, expected_state: StateIdentity) -> Result<(), CommandRejection> {
        let effect = self.model.close(expected_state)?;
        self.cancel_validation();
        self.drive(effect)?;
        self.finish_drain();
        Ok(())
    }
    pub fn quit(&mut self) {
        let effect = self.model.quit();
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
    fn drive(&mut self, mut effect: Option<ModelEffect>) -> Result<(), CommandRejection> {
        // Each event has at most one next effect. Synchronous no-resource failure
        // can advance through the single rollback, never create an unbounded retry.
        while let Some(next) = effect.take() {
            match next {
                ModelEffect::Validate(request) => {
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
                                SubmitFailure::CapacityUnavailable => {
                                    CommandRejection::CapacityUnavailable
                                }
                                SubmitFailure::Disconnected => CommandRejection::Disconnected,
                            });
                        }
                    }
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
                    });
                    match self.runner.begin_open(key, prepared, self.gain) {
                        Ok(()) => {}
                        Err(StartFailure::NoResourcesCreated(failure)) => {
                            let _ = self.model.open_failed(key, failure);
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
            SessionEvent::OpenVerified { key, receipt } => self.commit_receipt(key, receipt),
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
            SessionEvent::SessionEnded { attempt } => {
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
                    "session_ended",
                    "capture session ended",
                );
                let effect = self.model.session_failed(attempt, failure);
                let _ = self.drive(effect);
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
            SessionEvent::AudioDiagnostic { attempt, status } => {
                if !self.known_attempt(attempt)
                    || self.lease.as_ref().is_some_and(|lease| lease.stopping)
                {
                    return;
                }
                if let AudioDiagnostic::RestartRequired(error) = &status
                    && let Some((key, settings)) = self.model.opening()
                {
                    let failure = ApplyFailure::new(
                        FailureCategory::Session,
                        Stage::Open,
                        Cause::Generic,
                        settings.clone(),
                        "audio_open",
                        error.to_string(),
                    );
                    let effect = self.model.open_failed(key, failure);
                    let _ = self.drive(effect);
                }
                self.audio = Some(status);
            }
        }
    }
    fn commit_receipt(&mut self, key: AttemptKey, receipt: OpenReceipt) {
        if !self.known_key(key) || self.model.opening().map(|(current, _)| current) != Some(key) {
            return;
        }
        let Some((_, settings)) = self.model.opening() else {
            return;
        };
        if !receipt.matches(settings) {
            let failure = Self::protocol_failure(
                settings.clone(),
                "open_receipt",
                "receipt settings or requested audio outcome mismatch",
            );
            let effect = self.model.open_failed(key, failure);
            let _ = self.drive(effect);
            return;
        }
        self.model.open_verified(key);
        if let Some(lease) = &mut self.lease {
            lease.verified = true;
        }
        self.audio = Some(if receipt.settings.audio.enabled() {
            AudioDiagnostic::Active
        } else {
            AudioDiagnostic::Disabled
        });
    }
    pub fn poll(&mut self) {
        let mut ready = None;
        // One bounded batch. Terminal resource/health events beat readiness even
        // if a faulty adapter presents a cached Ready before its acknowledgement.
        for _ in 0..16 {
            let Some(event) = self.runner.poll() else {
                break;
            };
            match event {
                SessionEvent::OpenVerified { key, receipt } if self.known_key(key) => {
                    if self.model.opening().map(|(current, _)| current) == Some(key) {
                        ready = Some((key, receipt));
                    }
                }
                other => self.handle_event(other),
            }
        }
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
                if result.request != request {
                    let failure = Self::protocol_failure(
                        request.settings.clone(),
                        "validation_result",
                        "terminal validation identity mismatch",
                    );
                    self.model.validation_failed(request, failure);
                } else {
                    match result.result {
                        Ok(prepared) => {
                            self.prepared = Some((request.clone(), prepared));
                            let effect = self.model.validation_succeeded(&request);
                            let _ = self.drive(effect);
                        }
                        Err(failure) => self.model.validation_failed(request, failure),
                    }
                }
            }
        }
        self.finish_drain();
    }
    fn finish_drain(&mut self) {
        let retired = self.validator_shutdown && self.validator.shutdown_complete();
        self.model
            .drain_complete(self.validation.is_none(), retired);
    }
    pub fn submit_immediate(
        &mut self,
        attempt: AttemptId,
        intent: ImmediateIntent,
    ) -> SubmitStatus {
        if self.model.active().map(|active| active.attempt()) != Some(attempt) {
            return match &self.lease {
                Some(lease) if lease.key.attempt == attempt && lease.stopping => {
                    SubmitStatus::Closing
                }
                Some(lease) if lease.key.attempt == attempt => SubmitStatus::NotReady,
                _ => SubmitStatus::StaleGeneration,
            };
        }
        let status = self.runner.submit_immediate(attempt, intent);
        if status == SubmitStatus::Accepted
            && let ImmediateIntent::SetGain(gain) = intent
        {
            self.gain = gain;
        }
        status
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
        state::{AttemptPurpose, CleanupStatus, ProductPhase, ValidationKey},
    };

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
        }
    }
    #[derive(Default)]
    struct Validator {
        request: Option<ValidationRequest>,
        result: Option<ValidationResult<DraftSettings>>,
        requests: Vec<ValidationRequest>,
        cancelled: bool,
        shutdown: bool,
        retired: bool,
        reject: Option<SubmitFailure>,
    }
    impl Validator {
        fn finish(&mut self, result: Result<(), ApplyFailure>) {
            let request = self.request.take().unwrap();
            self.result = Some(ValidationResult {
                result: result.map(|()| request.settings.clone()),
                request,
            });
        }
    }
    impl DraftValidator for Validator {
        type Prepared = DraftSettings;
        fn begin_validate(&mut self, request: ValidationRequest) -> Result<(), SubmitFailure> {
            if let Some(rejection) = self.reject.take() {
                return Err(rejection);
            }
            if self.request.is_some() || self.result.is_some() {
                return Err(SubmitFailure::CapacityUnavailable);
            }
            self.requests.push(request.clone());
            self.request = Some(request);
            Ok(())
        }
        fn poll_validation(&mut self) -> Option<ValidationResult<Self::Prepared>> {
            self.result.take()
        }
        fn cancel_validation(&mut self, _: ValidationKey) {
            self.cancelled = true;
        }
        fn shutdown(&mut self) {
            self.shutdown = true;
        }
        fn shutdown_complete(&mut self) -> bool {
            self.retired
        }
    }
    #[derive(Default)]
    struct Runner {
        events: VecDeque<SessionEvent>,
        opens: Vec<(AttemptKey, DraftSettings, PlaybackGain)>,
        stops: Vec<(AttemptId, StopReason)>,
        immediate: Option<SubmitStatus>,
        start_failure: Option<StartFailure>,
        stop_failure: Option<StopSubmission>,
    }
    impl Runner {
        fn verified(&mut self) {
            let (key, settings, _) = self.opens.last().unwrap();
            self.events.push_back(SessionEvent::OpenVerified {
                key: *key,
                receipt: receipt(settings.clone()),
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
        type Prepared = DraftSettings;
        fn begin_open(
            &mut self,
            key: AttemptKey,
            prepared: DraftSettings,
            gain: PlaybackGain,
        ) -> Result<(), StartFailure> {
            self.opens.push((key, prepared, gain));
            self.start_failure.take().map_or(Ok(()), Err)
        }
        fn stop(&mut self, attempt: AttemptId, reason: StopReason) -> StopSubmission {
            self.stops.push((attempt, reason));
            self.stop_failure.take().unwrap_or(StopSubmission::Accepted)
        }
        fn poll(&mut self) -> Option<SessionEvent> {
            self.events.pop_front()
        }
        fn submit_immediate(&mut self, _: AttemptId, _: ImmediateIntent) -> SubmitStatus {
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
        assert_eq!(
            engine.submit_immediate(old, ImmediateIntent::SetGain(gain)),
            SubmitStatus::Accepted
        );
        engine.runner_mut().immediate = Some(SubmitStatus::CapacityExceeded);
        assert_eq!(
            engine.submit_immediate(old, ImmediateIntent::SetGain(PlaybackGain::default())),
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
                receipt: receipt(settings(60)),
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
                            receipt: stale,
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
                receipt: receipt(settings(30)),
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
                    SessionEvent::SessionEnded { attempt: old }
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
            opened.audio = match outcome {
                0 => AudioOutcome::Disabled,
                1 => AudioOutcome::Active {
                    source: AudioSourceIdentity::new("wrong".into(), vec![]).unwrap(),
                },
                _ => AudioOutcome::Active { source },
            };
            engine
                .runner_mut()
                .events
                .push_back(SessionEvent::OpenVerified {
                    key,
                    receipt: opened,
                });
            engine.poll();
            assert_eq!(engine.model().active().is_some(), outcome == 2);
            assert_eq!(engine.model().last_valid().is_some(), outcome == 2);
        }
    }

    #[test]
    fn audio_restart_required_before_receipt_fails_candidate_but_missing_video_observations_remain_unverified()
     {
        let mut engine = engine();
        apply(&mut engine);
        validate(&mut engine);
        let key = engine.runner_mut().opens[0].0;
        engine.runner_mut().verified();
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::AudioDiagnostic {
                attempt: key.attempt,
                status: AudioDiagnostic::RestartRequired(
                    crate::domain::capture::AudioError::RecordingLost {
                        detail: "transport".into(),
                    },
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
                receipt: opened,
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
            request,
            result: Ok(settings(30)),
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
        engine.validator_mut().finish(Ok(()));
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Stopped);
        engine.reconnect(engine.model().state_identity()).unwrap();
        validate(&mut engine);
        let current = engine.runner_mut().opens.last().unwrap().0.attempt;
        engine.runner_mut().verified();
        engine.poll();
        engine
            .runner_mut()
            .events
            .push_back(SessionEvent::AudioDiagnostic {
                attempt: old,
                status: AudioDiagnostic::RestartRequired(
                    crate::domain::capture::AudioError::Cancelled,
                ),
            });
        engine.poll();
        assert_eq!(engine.audio_diagnostic(), Some(&AudioDiagnostic::Disabled));
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
}
