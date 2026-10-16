//! Serialized product orchestration over opaque validation and session ports.

use crate::domain::{
    capture::{
        AudioAvailability, AudioEpoch, AudioSelection, AudioSilence, LossEvidence, PlaybackGain,
        RecoveryObservation, RecoveryWatchTarget, SelectionToken, SourcePresence, WatchStamp,
    },
    failure::{ApplyFailure, Cause, FailureCategory, LifecycleFailure, Stage, ValidationLayer},
    output::{OutputPlan, OutputRevision, OutputSilence},
    state::{
        AppliedSettings, ApplyAdmission, ApplyId, AttemptId, AttemptKey, AttemptPurpose,
        CleanupStatus, CommandRejection, DraftRevision, DraftSettings, InitialPlayback,
        ModelEffect, PlaybackState, ProductModel, ProductPhase, ReconnectAdmission, StateIdentity,
        StopIntent, TreatmentAttemptKey, TreatmentConfirmation, TreatmentPass,
        TreatmentRestoreRoute, ValidationRequest,
    },
};

use super::diagnostic::{
    DiagnosticFailure, DiagnosticFailureContexts, FailureContextState, Mutation,
};
use super::ports::{
    AudioOutcome, DraftValidator, ImmediateIntent, OpenReadiness, OpenReceipt, SessionEvent,
    SessionRunner, StartFailure, StopReason, StopSubmission, SubmitFailure, SubmitStatus,
    ValidationOutcome,
};

// The model remains the sole transition authority. Each observed call runs once;
// companions are committed before drive can synchronously retire or roll back.
macro_rules! diagnostic_model {
    ($coordinator:ident, $mutation:expr, $method:ident($($argument:expr),* $(,)?)) => {{
        let mutation = $mutation;
        $coordinator.diagnostic.record(&mut $coordinator.model, mutation, None,
            |model| model.$method($($argument),*))
    }};
}
macro_rules! diagnostic_failure {
    ($coordinator:ident, $mutation:expr, $failure:ident, $method:ident($($argument:expr),* $(,)?)) => {{
        let mutation = $mutation;
        let frozen = DiagnosticFailure::from(&$failure);
        $coordinator.diagnostic.record(&mut $coordinator.model, mutation, Some(frozen),
            |model| model.$method($($argument),*))
    }};
}

/// A complete runtime-verified configuration. Consumers must independently
/// correlate it with an immutable user admission before authorizing a save.
#[derive(Clone, Debug, PartialEq)]
pub enum VerifiedApplied {
    Open {
        key: AttemptKey,
        applied: AppliedSettings,
    },
    Treatments {
        key: TreatmentAttemptKey,
        revision: DraftRevision,
        applied: AppliedSettings,
    },
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

struct PreparedTreatmentApply<F> {
    key: TreatmentAttemptKey,
    candidate: Option<F>,
    prior: Option<F>,
}

pub struct ApplyCoordinator<V, R>
where
    V: DraftValidator,
    R: SessionRunner<Prepared = V::Prepared>,
{
    model: ProductModel,
    diagnostic: FailureContextState,
    validator: V,
    runner: R,
    gain: PlaybackGain,
    output: OutputPlan,
    output_initialized: bool,
    validation: Option<ValidationRequest>,
    prepared: Option<(ValidationRequest, V::Prepared, R::PreparedTreatments)>,
    lease: Option<Lease>,
    audio: Option<AudioAvailability>,
    validator_shutdown: bool,
    installed_watch: Option<RecoveryWatchTarget>,
    pending_validation: Option<ValidationRequest>,
    deferred_ready: Option<(AttemptKey, OpenReceipt)>,
    verified_applied: Option<VerifiedApplied>,
    admitted_apply: Option<ApplyId>,
    strict_restore_apply: Option<ApplyId>,
    treatment_apply: Option<PreparedTreatmentApply<R::PreparedTreatments>>,
    // One-owner event arbitration may record model failures before blockers,
    // but adapter effects (including resource-free retirement) wait for them.
    defer_effects: bool,
    deferred_effect: Option<ModelEffect>,
}
impl<V, R> ApplyCoordinator<V, R>
where
    V: DraftValidator,
    R: SessionRunner<Prepared = V::Prepared>,
{
    pub fn new(settings: DraftSettings, gain: PlaybackGain, validator: V, runner: R) -> Self {
        Self {
            model: ProductModel::new(settings),
            diagnostic: FailureContextState::default(),
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
            verified_applied: None,
            admitted_apply: None,
            strict_restore_apply: None,
            treatment_apply: None,
            defer_effects: false,
            deferred_effect: None,
        }
    }
    pub fn model(&self) -> &ProductModel {
        &self.model
    }
    pub(crate) fn diagnostic_failures(&self) -> &DiagnosticFailureContexts {
        self.diagnostic.visible()
    }
    pub fn validator_mut(&mut self) -> &mut V {
        &mut self.validator
    }
    pub fn runner(&self) -> &R {
        &self.runner
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
    /// Consume the latest verified configuration once. Draft edits preserve it;
    /// health loss, replacement, close and quit revoke an unconsumed result.
    pub fn take_verified_applied(&mut self) -> Option<VerifiedApplied> {
        if self
            .verified_applied
            .as_ref()
            .is_some_and(|event| !self.positive_is_valid(event))
        {
            self.verified_applied = None;
        }
        let result = self.verified_applied.take();
        if result.is_some() {
            self.admitted_apply = None;
        }
        result
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
        self.strict_restore_apply = Some(apply);
        Ok(())
    }
    /// Restoring a source profile keeps its explicit enabled input strict even
    /// when this Candidate replaces an incumbent. It never authorizes rollback.
    pub fn require_profile_restore_audio(
        &mut self,
        apply: ApplyId,
    ) -> Result<(), CommandRejection> {
        self.check_drain()?;
        if !self.restore_candidate(apply) {
            return Err(CommandRejection::StaleState);
        }
        self.strict_restore_apply = Some(apply);
        Ok(())
    }
    fn startup_restore_candidate(&self, apply: ApplyId) -> bool {
        self.model.last_valid().is_none() && self.restore_candidate(apply)
    }
    fn restore_candidate(&self, apply: ApplyId) -> bool {
        self.model.has_pending_user_apply(apply) && self.model.treating().is_none()
    }
    fn retire_startup_restore_guard(&mut self) {
        if self
            .strict_restore_apply
            .is_some_and(|apply| !self.restore_candidate(apply))
        {
            self.strict_restore_apply = None;
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
    ) -> Result<ApplyAdmission, CommandRejection> {
        self.apply_with_source_binding(expected_state, expected_revision, None)
    }
    /// Applies the draft to a catalog-selected session-only physical capture route.
    /// The borrowed pair is node syspath first, USB syspath second.
    pub fn apply_selected_capture(
        &mut self,
        expected_state: StateIdentity,
        expected_revision: DraftRevision,
        physical_node: (&std::path::Path, &std::path::Path),
    ) -> Result<ApplyAdmission, CommandRejection> {
        self.apply_with_source_binding(expected_state, expected_revision, Some(physical_node))
    }
    /// Checks the common admission guards before a UI-only partial draft is installed.
    pub(crate) fn check_apply_ready(
        &self,
        expected_state: StateIdentity,
        expected_revision: DraftRevision,
    ) -> Result<(), CommandRejection> {
        self.check_drain()?;
        self.model.check_command(expected_state)?;
        if expected_revision != self.model.draft().revision {
            return Err(CommandRejection::StaleRevision);
        }
        Ok(())
    }
    fn apply_with_source_binding(
        &mut self,
        expected_state: StateIdentity,
        expected_revision: DraftRevision,
        physical_node: Option<(&std::path::Path, &std::path::Path)>,
    ) -> Result<ApplyAdmission, CommandRejection> {
        self.check_apply_ready(expected_state, expected_revision)?;
        let draft = &self.model.draft().settings;
        if self.model.active().is_some_and(|active| {
            active.applied().settings().video.same_request(&draft.video)
                && active.applied().settings().audio == draft.audio
                && physical_node
                    .is_none_or(|binding| active.applied().physical_node() == Some(binding))
                && matches!(
                    active.playback(),
                    PlaybackState::Live | PlaybackState::Paused
                )
        }) {
            return self.apply_treatments(expected_state, expected_revision);
        }
        let (apply, effect) = match physical_node {
            Some(binding) => diagnostic_model!(
                self,
                Mutation::Admission,
                apply_selected_capture(expected_state, expected_revision, binding)
            )?,
            None => diagnostic_model!(
                self,
                Mutation::Admission,
                apply(expected_state, expected_revision)
            )?,
        };
        self.verified_applied = None;
        self.admitted_apply = Some(apply);
        self.drive(Some(effect))?;
        Ok(ApplyAdmission::Open { apply })
    }
    pub fn apply_treatments(
        &mut self,
        expected_state: StateIdentity,
        expected_revision: DraftRevision,
    ) -> Result<ApplyAdmission, CommandRejection> {
        self.check_drain()?;
        self.model.check_command(expected_state)?;
        if self.validation.is_some() || self.pending_validation.is_some() {
            return Err(CommandRejection::CleanupIncomplete);
        }
        let active = self
            .model
            .active()
            .ok_or(CommandRejection::PlaybackUnavailable)?;
        if self.lease.as_ref().is_none_or(|lease| {
            lease.key.attempt != active.attempt()
                || lease.stopping
                || lease.owner_stopped
                || lease.blocked
                || !lease.verified
        }) {
            return Err(CommandRejection::PlaybackUnavailable);
        }
        let admission = diagnostic_model!(
            self,
            Mutation::TreatmentAdmission,
            apply_treatments(expected_state, expected_revision)
        )?;
        let ApplyAdmission::Treatments { key, .. } = admission else {
            unreachable!()
        };
        self.verified_applied = None;
        self.admitted_apply = Some(admission.id());
        let transition = self
            .model
            .treating()
            .expect("admitted treatment transition");
        let candidate = transition.candidate().clone();
        let prior = transition.prior().settings().clone();
        let prepared = self
            .runner
            .prepare_treatments(&candidate)
            .and_then(|candidate| {
                self.runner
                    .prepare_treatments(&prior)
                    .map(|prior| (candidate, prior))
            });
        match prepared {
            Ok((candidate, prior)) => {
                self.treatment_apply = Some(PreparedTreatmentApply {
                    key,
                    candidate: Some(candidate),
                    prior: Some(prior),
                });
                self.drive(Some(ModelEffect::ApplyTreatments { key }))?;
            }
            Err(mut failure) => {
                if let Some(treatment) = &mut failure.treatment {
                    treatment.key = Some(key);
                }
                diagnostic_failure!(
                    self,
                    Mutation::TreatmentRefused(key),
                    failure,
                    treatment_submission_refused(key, failure)
                );
            }
        }
        Ok(admission)
    }
    /// Authorization is independent of any later receipt-derived owner.
    pub fn has_user_apply_result_or_pending(&self, apply: ApplyId) -> bool {
        self.admitted_apply == Some(apply)
            && (self.model.has_pending_user_apply(apply)
                || self.verified_applied.as_ref().is_some_and(|event| {
                    let id = match event {
                        VerifiedApplied::Open { key, .. } => key.apply,
                        VerifiedApplied::Treatments { key, .. } => key.apply,
                    };
                    id == apply && self.positive_is_valid(event)
                }))
    }
    fn positive_is_valid(&self, event: &VerifiedApplied) -> bool {
        let (key, applied) = match event {
            VerifiedApplied::Open { key, applied } if key.purpose == AttemptPurpose::Candidate => (
                TreatmentAttemptKey {
                    apply: key.apply,
                    attempt: key.attempt,
                    pass: TreatmentPass::Open,
                },
                applied,
            ),
            VerifiedApplied::Treatments { key, applied, .. }
                if key.pass == TreatmentPass::Candidate =>
            {
                (*key, applied)
            }
            _ => return false,
        };
        self.model.confirmed_treatment_key() == Some(key)
            && self.model.treating().is_none()
            && self.model.cleanup() == &CleanupStatus::Complete
            && self.model.active().is_some_and(|active| {
                active.attempt() == key.attempt && active.applied() == applied
            })
            && self.lease.as_ref().is_some_and(|lease| {
                lease.key.attempt == key.attempt
                    && lease.verified
                    && !lease.stopping
                    && !lease.owner_stopped
                    && !lease.blocked
            })
    }
    pub fn restart(&mut self, expected_state: StateIdentity) -> Result<ApplyId, CommandRejection> {
        self.check_drain()?;
        let (id, effect) = diagnostic_model!(self, Mutation::Admission, restart(expected_state))?;
        self.verified_applied = None;
        self.admitted_apply = None;
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
        let (admission, effect) =
            diagnostic_model!(self, Mutation::Admission, reconnect(expected_state))?;
        if matches!(admission, ReconnectAdmission::Started(_)) {
            self.verified_applied = None;
        }
        if matches!(admission, ReconnectAdmission::Started(_)) {
            self.admitted_apply = None;
        }
        self.drive(effect)?;
        Ok(admission)
    }
    pub fn choose_recovery(
        &mut self,
        expected_state: StateIdentity,
        token: SelectionToken,
    ) -> Result<(), CommandRejection> {
        self.check_drain()?;
        let effect = diagnostic_model!(
            self,
            Mutation::Admission,
            choose_recovery(expected_state, token)
        )?;
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
        } else {
            self.model.cancel_prepared_pause(attempt, request);
        }
        Ok(status)
    }
    pub fn resume(
        &mut self,
        expected_state: StateIdentity,
        attempt: AttemptId,
    ) -> Result<ApplyId, CommandRejection> {
        self.check_playback()?;
        let (id, effect) =
            diagnostic_model!(self, Mutation::Admission, resume(expected_state, attempt))?;
        self.verified_applied = None;
        self.admitted_apply = None;
        self.drive(Some(effect))?;
        Ok(id)
    }
    fn check_playback(&self) -> Result<(), CommandRejection> {
        self.check_drain()
    }
    pub fn close(&mut self, expected_state: StateIdentity) -> Result<(), CommandRejection> {
        let effect = diagnostic_model!(self, Mutation::Stop, close(expected_state))?;
        self.validator.clear_watch();
        self.installed_watch = None;
        self.cancel_validation();
        self.drive(effect)?;
        self.finish_drain();
        Ok(())
    }
    pub fn quit(&mut self) {
        let effect = diagnostic_model!(self, Mutation::Stop, quit());
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
        self.treatment_apply = None;
        self.verified_applied = None;
        self.admitted_apply = None;
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
                diagnostic_failure!(
                    self,
                    Mutation::validation(&self.model, &request),
                    failure,
                    validation_failed(request, failure)
                );
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
            diagnostic_failure!(
                self,
                Mutation::validation(&self.model, &request),
                failure,
                validation_failed(request, failure)
            );
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
                diagnostic_failure!(
                    self,
                    Mutation::validation(&self.model, &request),
                    failure,
                    validation_failed(request, failure)
                );
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
        let effect = diagnostic_model!(
            self,
            Mutation::observation(&observation),
            recovery_observed(&observation)
        );
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
    fn treatment_failure(
        settings: DraftSettings,
        failure: crate::domain::failure::TreatmentFailure,
    ) -> ApplyFailure {
        ApplyFailure::new(
            FailureCategory::Session,
            Stage::Verification,
            Cause::Generic,
            settings,
            "treatment_verification",
            match &failure.kind {
                crate::domain::failure::TreatmentErrorKind::Renderer(renderer) => {
                    renderer.diagnostic.clone()
                }
                _ => format!("{:?}", failure.kind),
            },
        )
        .with_treatment(failure)
    }
    fn treatment_unavailable(
        key: TreatmentAttemptKey,
        settings: DraftSettings,
        detail: &str,
    ) -> ApplyFailure {
        let mut failure = Self::treatment_failure(
            settings,
            crate::domain::failure::TreatmentFailure {
                key: Some(key),
                kind: crate::domain::failure::TreatmentErrorKind::Unconfirmed {
                    reason:
                        crate::domain::failure::TreatmentConfirmationFailure::BackendUnavailable,
                },
                disposition: crate::domain::failure::TreatmentFailureDisposition::RestoreFreshOwner,
            },
        );
        failure.diagnostic = detail.to_owned();
        failure
    }
    fn current_treatment_key(&self, key: TreatmentAttemptKey) -> bool {
        if !self.known_attempt(key.attempt) {
            return false;
        }
        if let Some(transition) = self.model.treating() {
            return transition.key() == key
                || self.model.confirmed_treatment_key() == Some(key)
                || (transition.route() == Some(TreatmentRestoreRoute::FreshOwner)
                    && transition.key().apply == key.apply
                    && key.pass == TreatmentPass::Candidate);
        }
        if key.pass == TreatmentPass::Open
            && self
                .model
                .opening()
                .is_some_and(|(open, _)| open.apply == key.apply && open.attempt == key.attempt)
        {
            return true;
        }
        // Previous-owner proof is diagnostic-only once source loss is frozen.
        // A fresh owner's exact Open key remains eligible above.
        self.model.confirmed_treatment_key() == Some(key)
            && self.model.recovery().is_none()
            && matches!(
                self.model.phase(),
                ProductPhase::Active
                    | ProductPhase::Paused
                    | ProductPhase::PausePending
                    | ProductPhase::ErrorWithActiveRestored
                    | ProductPhase::Validating
                    | ProductPhase::ClosingOld
                    | ProductPhase::ValidatingResume
                    | ProductPhase::ClosingResume
            )
    }
    fn event_treatment_key(&self, event: &SessionEvent) -> Option<TreatmentAttemptKey> {
        let key = match event {
            SessionEvent::TreatmentResult {
                key,
                result: Err(failure),
            } if failure.key == Some(*key) => *key,
            SessionEvent::TreatmentFault { key, failure } if failure.key == Some(*key) => *key,
            SessionEvent::SessionFailed { failure, .. }
            | SessionEvent::OpenFailed { failure, .. } => failure.treatment.as_ref()?.key?,
            _ => return None,
        };
        self.current_treatment_key(key).then_some(key)
    }
    fn handle_stream_ended(
        &mut self,
        attempt: AttemptId,
        reason: i32,
        error: i32,
        treatment_terminal: bool,
    ) {
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
        self.verified_applied = None;
        let effect = if let Some(transition) = self.model.treating().filter(|transition| {
            treatment_terminal || transition.key().pass == TreatmentPass::Restore
        }) {
            let failure = if transition.route() == Some(TreatmentRestoreRoute::FreshOwner) {
                failure
            } else {
                Self::treatment_unavailable(
                    transition.key(),
                    transition.prior().settings().clone(),
                    &failure.diagnostic,
                )
            };
            diagnostic_failure!(
                self,
                Mutation::Session(attempt, None),
                failure,
                session_failed(attempt, failure)
            )
        } else if treatment_terminal {
            // The exact keyed negative already failed an opening/restore or
            // revoked this owner. Its EndFile is not a second recovery trigger.
            None
        } else {
            diagnostic_failure!(
                self,
                Mutation::VideoLoss(attempt),
                failure,
                video_lost(
                    attempt,
                    LossEvidence::StreamEnded { reason, error },
                    failure
                )
            )
        };
        let _ = self.drive(effect);
    }
    fn handle_treatment_failure(
        &mut self,
        key: TreatmentAttemptKey,
        failure: crate::domain::failure::TreatmentFailure,
        fault: bool,
    ) {
        if failure.key != Some(key) || !self.known_attempt(key.attempt) {
            return;
        }
        let transition = self.model.treating();
        let effect = if let Some(transition) = transition {
            if key.attempt != transition.key().attempt {
                return;
            }
            if !fault && transition.route() == Some(TreatmentRestoreRoute::FreshOwner) {
                return;
            }
            let current = transition.key();
            let matching = key == current
                || (fault
                    && key.apply == current.apply
                    && key.pass == TreatmentPass::Candidate
                    && transition.route() == Some(TreatmentRestoreRoute::FreshOwner));
            let incumbent_fault = fault && self.model.confirmed_treatment_key() == Some(key);
            if !matching && !incumbent_fault {
                return;
            }
            if fault && transition.route() == Some(TreatmentRestoreRoute::FreshOwner) {
                let settings = if key.pass == TreatmentPass::Candidate && key.apply == current.apply
                {
                    transition.candidate().clone()
                } else {
                    transition.prior().settings().clone()
                };
                let failure = Self::treatment_failure(settings, failure);
                diagnostic_failure!(
                    self,
                    Mutation::Incumbent(key),
                    failure,
                    treatment_incumbent_failed(failure)
                );
                return;
            }
            let settings = if key.pass == TreatmentPass::Candidate && !incumbent_fault {
                transition.candidate().clone()
            } else {
                transition.prior().settings().clone()
            };
            let failure = Self::treatment_failure(settings, failure);
            if incumbent_fault {
                let requested = if current.pass == TreatmentPass::Candidate {
                    self.model
                        .treating()
                        .expect("current transition")
                        .candidate()
                        .clone()
                } else {
                    self.model
                        .treating()
                        .expect("current transition")
                        .prior()
                        .settings()
                        .clone()
                };
                diagnostic_failure!(
                    self,
                    Mutation::Incumbent(key),
                    failure,
                    treatment_incumbent_failed(failure)
                );
                let failure = Self::treatment_unavailable(
                    current,
                    requested,
                    "incumbent treatment proof revoked",
                );
                diagnostic_failure!(
                    self,
                    Mutation::Treatment(current),
                    failure,
                    treatment_failed(current, failure, true)
                )
            } else {
                diagnostic_failure!(
                    self,
                    Mutation::Treatment(key),
                    failure,
                    treatment_failed(key, failure, fault)
                )
            }
        } else if self.model.confirmed_treatment_key() == Some(key) {
            self.verified_applied = None;
            let settings = self
                .model
                .last_valid()
                .expect("confirmed renderer has applied state")
                .settings()
                .clone();
            let failure = Self::treatment_failure(settings, failure);
            let effect = diagnostic_failure!(
                self,
                Mutation::LateTreatment(key),
                failure,
                late_treatment_failed(key, failure)
            );
            if effect.is_some() {
                self.cancel_validation();
            }
            effect
        } else if key.pass == TreatmentPass::Open
            && self
                .model
                .opening()
                .is_some_and(|(open, _)| open.apply == key.apply && open.attempt == key.attempt)
        {
            let (open, settings) = self.model.opening().expect("matched opening");
            let failure = Self::treatment_failure(settings.clone(), failure);
            diagnostic_failure!(
                self,
                Mutation::Open(open),
                failure,
                open_failed(open, failure)
            )
        } else {
            return;
        };
        let _ = self.drive(effect);
    }
    fn commit_treatment_confirmation(
        &mut self,
        key: TreatmentAttemptKey,
        confirmation: TreatmentConfirmation,
    ) {
        if key != confirmation.key() {
            return;
        }
        if self.lease.as_ref().is_none_or(|lease| {
            lease.key.attempt != key.attempt
                || lease.stopping
                || lease.owner_stopped
                || lease.blocked
        }) {
            return;
        }
        let revision = self
            .model
            .treating()
            .map(|transition| transition.revision());
        if diagnostic_model!(self, Mutation::Confirmed, treatment_confirmed(confirmation)) {
            if let (Some(lease), Some(active)) = (&mut self.lease, self.model.active()) {
                lease.settings = active.applied().settings().clone();
            }
            self.treatment_apply = None;
            self.verified_applied = self.model.active().and_then(|active| {
                (key.pass == TreatmentPass::Candidate).then(|| VerifiedApplied::Treatments {
                    key,
                    revision: revision.expect("confirmed frozen treatment transition"),
                    applied: active.applied().clone(),
                })
            });
        }
    }
    fn drive(&mut self, mut effect: Option<ModelEffect>) -> Result<(), CommandRejection> {
        if self.defer_effects {
            if let Some(ModelEffect::Stop { attempt, .. }) = &effect
                && let Some(lease) = self
                    .lease
                    .as_mut()
                    .filter(|lease| lease.key.attempt == *attempt)
            {
                // Cleanup is logically selected now; physical stop/retirement
                // still waits behind the blocker fence and actual owner facts.
                lease.stopping = true;
            }
            if effect.is_some() {
                self.deferred_effect = effect;
            }
            return Ok(());
        }
        self.retire_startup_restore_guard();
        // Each event has at most one next effect. Synchronous no-resource failure
        // can advance through the single rollback, never create an unbounded retry.
        while let Some(next) = effect.take() {
            match next {
                ModelEffect::ApplyTreatments { key } => {
                    let usable = self.lease.as_ref().is_some_and(|lease| {
                        lease.key.attempt == key.attempt
                            && lease.verified
                            && !lease.stopping
                            && !lease.owner_stopped
                            && !lease.blocked
                    });
                    let settings = self.model.treating().map(|transition| {
                        if key.pass == TreatmentPass::Candidate {
                            transition.candidate().clone()
                        } else {
                            transition.prior().settings().clone()
                        }
                    });
                    let Some(settings) = settings else { continue };
                    if !usable {
                        let failure = Self::treatment_unavailable(
                            key,
                            settings,
                            "selected treatment owner unavailable",
                        );
                        effect = diagnostic_failure!(
                            self,
                            Mutation::Treatment(key),
                            failure,
                            treatment_failed(key, failure, true)
                        );
                        continue;
                    }
                    let prepared = self
                        .treatment_apply
                        .as_mut()
                        .filter(|slot| {
                            slot.key.apply == key.apply && slot.key.attempt == key.attempt
                        })
                        .and_then(|slot| match key.pass {
                            TreatmentPass::Candidate => slot.candidate.take(),
                            TreatmentPass::Restore => slot.prior.take(),
                            TreatmentPass::Open => None,
                        });
                    let Some(prepared) = prepared else {
                        let failure = Self::treatment_unavailable(
                            key,
                            settings,
                            "matching prepared treatment request unavailable",
                        );
                        effect = diagnostic_failure!(
                            self,
                            Mutation::TreatmentRefused(key),
                            failure,
                            treatment_submission_refused(key, failure)
                        );
                        continue;
                    };
                    let status = self.runner.submit_treatments(key, prepared);
                    if status != SubmitStatus::Accepted {
                        let mut failure = Self::treatment_unavailable(
                            key,
                            settings,
                            &format!("treatment admission: {status:?}"),
                        );
                        if let Some(treatment) = &mut failure.treatment {
                            treatment.disposition = if matches!(
                                status,
                                SubmitStatus::StaleGeneration
                                    | SubmitStatus::NotReady
                                    | SubmitStatus::Closing
                            ) {
                                crate::domain::failure::TreatmentFailureDisposition::RestoreFreshOwner
                            } else {
                                crate::domain::failure::TreatmentFailureDisposition::Untouched
                            };
                        }
                        effect = if key.pass == TreatmentPass::Candidate
                            && matches!(
                                status,
                                SubmitStatus::StaleGeneration
                                    | SubmitStatus::NotReady
                                    | SubmitStatus::Closing
                            ) {
                            diagnostic_failure!(
                                self,
                                Mutation::Treatment(key),
                                failure,
                                treatment_failed(key, failure, true)
                            )
                        } else {
                            diagnostic_failure!(
                                self,
                                Mutation::TreatmentRefused(key),
                                failure,
                                treatment_submission_refused(key, failure)
                            )
                        };
                    }
                }
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
                        diagnostic_failure!(
                            self,
                            Mutation::Cleanup(key.attempt),
                            failure,
                            cleanup_blocked(key.attempt, failure)
                        );
                        continue;
                    }
                    if let Some(next) =
                        diagnostic_model!(self, Mutation::Cutover(key), cutover_validation(key))
                    {
                        self.prepared = None;
                        effect = Some(next);
                        continue;
                    }
                    if self.model.opening().map(|(current, _)| current) != Some(key) {
                        self.prepared = None;
                        continue;
                    }
                    let prepared =
                        self.prepared
                            .take()
                            .and_then(|(identity, prepared, treatments)| {
                                (identity == request).then_some((prepared, treatments))
                            });
                    let Some((prepared, treatments)) = prepared else {
                        let failure = Self::protocol_failure(
                            request.settings,
                            "begin_open",
                            "matching prepared value unavailable",
                        );
                        let _ = diagnostic_failure!(
                            self,
                            Mutation::Open(key),
                            failure,
                            open_failed(key, failure)
                        );
                        if let Some(token) = request.choice {
                            self.validator.retire_selection(token);
                        }
                        effect = diagnostic_model!(
                            self,
                            Mutation::Barrier(key.attempt),
                            barrier_complete(key.attempt)
                        );
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
                        treatments,
                        self.gain,
                        request.playback,
                        self.output.clone(),
                    ) {
                        Ok(()) => {}
                        Err(StartFailure::NoResourcesCreated(failure)) => {
                            let _ = diagnostic_failure!(
                                self,
                                Mutation::Open(key),
                                failure,
                                open_failed(key, failure)
                            );
                            self.retire_lease_selection();
                            self.lease = None;
                            effect = diagnostic_model!(
                                self,
                                Mutation::Barrier(key.attempt),
                                barrier_complete(key.attempt)
                            );
                        }
                        Err(StartFailure::ResourcesCreated(failure)) => {
                            effect = diagnostic_failure!(
                                self,
                                Mutation::Open(key),
                                failure,
                                open_failed(key, failure)
                            );
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
                        diagnostic_failure!(
                            self,
                            Mutation::Cleanup(attempt),
                            failure,
                            cleanup_blocked(attempt, failure)
                        );
                        continue;
                    };
                    // A batched destruction acknowledgement may precede effect
                    // dispatch. Retain the native lease, but never command an
                    // owner already retired while awaiting native release.
                    if lease.owner_stopped {
                        continue;
                    }
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
                            effect = diagnostic_model!(
                                self,
                                Mutation::Barrier(attempt),
                                barrier_complete(attempt)
                            );
                        }
                        StopSubmission::NoResourcesCreated => {
                            let failure = Self::protocol_failure(
                                lease.settings.clone(),
                                "stop",
                                "verified session cannot report no resources created",
                            );
                            lease.blocked = true;
                            diagnostic_failure!(
                                self,
                                Mutation::Cleanup(attempt),
                                failure,
                                cleanup_blocked(attempt, failure)
                            );
                        }
                        StopSubmission::Blocked(failure) => {
                            lease.blocked = true;
                            diagnostic_failure!(
                                self,
                                Mutation::Cleanup(attempt),
                                failure,
                                cleanup_blocked(attempt, failure)
                            );
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
            SessionEvent::TreatmentResult {
                key,
                result: Err(failure),
            } => {
                self.handle_treatment_failure(key, *failure, false);
            }
            SessionEvent::TreatmentFault { key, failure } => {
                self.handle_treatment_failure(key, *failure, true);
            }
            SessionEvent::TreatmentResult {
                key,
                result: Ok(confirmation),
            } => {
                self.commit_treatment_confirmation(key, confirmation);
            }
            SessionEvent::OpenVerified { key, receipt } => self.commit_receipt(key, *receipt),
            SessionEvent::OpenFailed { key, mut failure } => {
                if !self.known_key(key) {
                    return;
                }
                if let Some(treatment_key) = failure
                    .treatment
                    .as_ref()
                    .and_then(|treatment| treatment.key)
                    && (self.current_treatment_key(treatment_key)
                        || self.model.confirmed_treatment_key() == Some(treatment_key))
                {
                    let mut treatment = failure.treatment.take().expect("keyed treatment failure");
                    treatment.disposition =
                        crate::domain::failure::TreatmentFailureDisposition::RestoreFreshOwner;
                    self.handle_treatment_failure(treatment_key, *treatment, false);
                    return;
                }
                let effect = diagnostic_failure!(
                    self,
                    Mutation::Open(key),
                    failure,
                    open_failed(key, failure)
                );
                let _ = self.drive(effect);
            }
            SessionEvent::SessionFailed {
                attempt,
                mut failure,
            } => {
                if !self.known_attempt(attempt) {
                    return;
                }
                self.verified_applied = None;
                if let Some(treatment_key) = failure
                    .treatment
                    .as_ref()
                    .and_then(|treatment| treatment.key)
                    && (self.current_treatment_key(treatment_key)
                        || self.model.confirmed_treatment_key() == Some(treatment_key))
                {
                    let mut treatment = failure.treatment.take().expect("keyed treatment failure");
                    treatment.disposition =
                        crate::domain::failure::TreatmentFailureDisposition::RestoreFreshOwner;
                    let fault = self.model.confirmed_treatment_key() == Some(treatment_key);
                    self.handle_treatment_failure(treatment_key, *treatment, fault);
                    return;
                }
                if let Some(transition) = self.model.treating().filter(|transition| {
                    transition.route() != Some(TreatmentRestoreRoute::FreshOwner)
                }) {
                    let key = transition.key();
                    let settings = if key.pass == TreatmentPass::Candidate {
                        transition.candidate().clone()
                    } else {
                        transition.prior().settings().clone()
                    };
                    if failure.treatment.is_none() {
                        let mut typed =
                            Self::treatment_unavailable(key, settings, &failure.diagnostic);
                        typed.category = failure.category;
                        failure = typed;
                    }
                }
                let effect = diagnostic_failure!(
                    self,
                    Mutation::Session(attempt, None),
                    failure,
                    session_failed(attempt, failure)
                );
                let _ = self.drive(effect);
            }
            SessionEvent::StreamEnded {
                attempt,
                reason,
                error,
            } => {
                self.handle_stream_ended(attempt, reason, error, false);
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
                if !lease.stopping
                    && (self.model.active().map(|active| active.attempt()) == Some(attempt)
                        || self
                            .model
                            .opening()
                            .is_some_and(|(key, _)| key.attempt == attempt))
                {
                    let mut failure = ApplyFailure::new(
                        FailureCategory::Lifecycle(LifecycleFailure::Acknowledgement),
                        Stage::Unknown,
                        Cause::Generic,
                        lease.settings.clone(),
                        "owner_stopped",
                        "owner stopped before product requested cleanup",
                    );
                    self.verified_applied = None;
                    if let Some(transition) = self.model.treating() {
                        let key = transition.key();
                        let settings = if key.pass == TreatmentPass::Candidate {
                            transition.candidate().clone()
                        } else {
                            transition.prior().settings().clone()
                        };
                        failure = Self::treatment_unavailable(key, settings, &failure.diagnostic);
                    }
                    // Owner already retired. Revoke live product state, without
                    // submitting another command to the destroyed owner.
                    let _ = diagnostic_failure!(
                        self,
                        Mutation::Session(attempt, None),
                        failure,
                        session_failed(attempt, failure)
                    );
                }
                if let Err(failure) = outcome {
                    diagnostic_failure!(
                        self,
                        Mutation::Cleanup(attempt),
                        failure,
                        cleanup_error(attempt, failure)
                    );
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
                let effect =
                    diagnostic_model!(self, Mutation::Barrier(attempt), barrier_complete(attempt));
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
                    diagnostic_failure!(
                        self,
                        Mutation::Cleanup(attempt),
                        failure,
                        cleanup_error(attempt, failure)
                    );
                    return;
                }
                lease.blocked = true;
                diagnostic_failure!(
                    self,
                    Mutation::Cleanup(attempt),
                    failure,
                    cleanup_blocked(attempt, failure)
                );
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
                        let effect = diagnostic_failure!(
                            self,
                            Mutation::Session(attempt, Some(epoch)),
                            failure,
                            session_failed(attempt, failure)
                        );
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
            match (request.playback, receipt.readiness) {
                (InitialPlayback::Live, OpenReadiness::Live) => true,
                (InitialPlayback::Paused, OpenReadiness::PausedPrepared) => {
                    matches!(
                        key.purpose,
                        AttemptPurpose::Recovery
                            | AttemptPurpose::Reconnect
                            | AttemptPurpose::Restore
                    ) && self
                        .model
                        .last_valid()
                        .is_some_and(|prior| prior.settings() == settings)
                }
                _ => false,
            }
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
        let startup_audio_matches = self.strict_restore_apply != Some(key.apply)
            || key.purpose != AttemptPurpose::Candidate
            || !settings.audio.enabled()
            || (self
                .model
                .observation()
                .zip(self.lease.as_ref())
                .is_some_and(|(observation, lease)| {
                    observation.stamp == lease.watch && observation.audio == SourcePresence::Present
                })
                && matches!(
                    &receipt.audio,
                    AudioOutcome::Active { .. }
                        | AudioOutcome::Silent {
                            reason: AudioSilence::Output(_),
                            ..
                        }
                ));
        if !receipt.matches(settings)
            || !receipt.matches_treatments(key)
            || !readiness_matches
            || !audio_matches
            || !startup_audio_matches
        {
            let failure = Self::protocol_failure(
                settings.clone(),
                "open_receipt",
                if startup_audio_matches {
                    "receipt settings, renderer proof or requested audio outcome mismatch"
                } else {
                    "profile or startup restore requires the exact healthy requested audio source"
                },
            );
            let effect = diagnostic_failure!(
                self,
                Mutation::Open(key),
                failure,
                open_failed(key, failure)
            );
            let _ = self.drive(effect);
            return;
        }
        diagnostic_model!(
            self,
            Mutation::Verified,
            open_verified(key, receipt.treatments.clone())
        );
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
                let effect = diagnostic_failure!(
                    self,
                    Mutation::Session(key.attempt, None),
                    failure,
                    session_failed(key.attempt, failure)
                );
                let _ = self.drive(effect);
            }
            return;
        }
        if let Some(active) = self.model.active().filter(|active| {
            active.attempt() == key.attempt
                && key.purpose == AttemptPurpose::Candidate
                && receipt.readiness == OpenReadiness::Live
                && self.admitted_apply == Some(key.apply)
        }) {
            self.verified_applied = Some(VerifiedApplied::Open {
                key,
                applied: active.applied().clone(),
            });
        }
    }
    pub fn poll(&mut self) {
        self.defer_effects = true;
        // Subscribe/observe health first; positive removals invalidate even a
        // coalesced Present before any cached media readiness can be committed.
        self.poll_observation();
        let mut ready = self.deferred_ready.take();
        // Stack-bounded drain: source cancellation first, settled rejection
        // plus independent health facts next, barriers then positive commits.
        let mut events: [Option<SessionEvent>; 16] = std::array::from_fn(|_| None);
        for slot in &mut events {
            *slot = self.runner.poll();
            if slot.is_none() {
                break;
            }
        }
        for slot in &mut events {
            if !matches!(
                slot,
                Some(
                    SessionEvent::TreatmentResult { .. }
                        | SessionEvent::TreatmentFault { .. }
                        | SessionEvent::OpenVerified { .. }
                        | SessionEvent::NativeReleased { .. }
                        | SessionEvent::OpenFailed { .. }
                        | SessionEvent::StreamEnded { .. }
                        | SessionEvent::CleanupBlocked { .. }
                        | SessionEvent::SessionFailed { .. }
                        | SessionEvent::OwnerStopped { .. }
                )
            ) && let Some(event) = slot.take()
            {
                self.handle_event(event);
            }
        }
        self.poll_observation();
        let treatment_terminal = events
            .iter()
            .flatten()
            .find_map(|event| self.event_treatment_key(event))
            .map(|key| key.attempt)
            .or_else(|| {
                self.model
                    .treating()
                    .filter(|transition| {
                        transition.route() == Some(TreatmentRestoreRoute::FreshOwner)
                    })
                    .map(|transition| transition.key().attempt)
            });
        for index in 0..events.len() {
            if let Some(SessionEvent::TreatmentResult {
                key,
                result: Err(mut failure),
            }) = events[index].take_if(|event| {
                matches!(event, SessionEvent::TreatmentResult { result: Err(_), .. })
            }) {
                let fault = events.iter().any(|event| {
                    let Some(SessionEvent::TreatmentFault {
                        key: fault_key,
                        failure,
                    }) = event
                    else {
                        return false;
                    };
                    if failure.key != Some(*fault_key) {
                        return false;
                    }
                    fault_key.attempt == key.attempt
                        && (*fault_key == key
                            || self.model.confirmed_treatment_key() == Some(*fault_key))
                });
                let terminal = events.iter().any(|event| matches!(event,
                    Some(SessionEvent::SessionFailed { attempt, .. } | SessionEvent::OwnerStopped { attempt, .. }
                        | SessionEvent::StreamEnded { attempt, .. })
                        if *attempt == key.attempt
                ));
                if fault || terminal {
                    failure.disposition =
                        crate::domain::failure::TreatmentFailureDisposition::RestoreFreshOwner;
                }
                if events.iter().any(|event| {
                    matches!(event,
                        Some(SessionEvent::OwnerStopped { attempt, .. }) if *attempt == key.attempt
                    )
                }) && let Some(lease) = self.lease.as_ref().filter(|lease| !lease.stopping)
                {
                    let diagnostic = ApplyFailure::new(
                        FailureCategory::Lifecycle(LifecycleFailure::Acknowledgement),
                        Stage::Unknown,
                        Cause::Generic,
                        lease.settings.clone(),
                        "owner_stopped",
                        "owner retired during treatment settlement",
                    );
                    let diagnostic_key = self
                        .model
                        .treating()
                        .map_or(key, |transition| transition.key());
                    diagnostic_failure!(
                        self,
                        Mutation::Incumbent(diagnostic_key),
                        diagnostic,
                        treatment_incumbent_failed(diagnostic)
                    );
                }
                self.handle_treatment_failure(key, *failure, false);
            }
        }
        for slot in &mut events {
            if matches!(slot, Some(SessionEvent::TreatmentFault { .. }))
                && let Some(event) = slot.take()
            {
                self.handle_event(event);
            }
        }
        for slot in &mut events {
            if matches!(
                slot,
                Some(
                    SessionEvent::OpenFailed { .. }
                        | SessionEvent::SessionFailed { .. }
                        | SessionEvent::OwnerStopped { .. }
                )
            ) && let Some(event) = slot.take()
            {
                self.handle_event(event);
            }
        }
        for slot in &mut events {
            if let Some(SessionEvent::StreamEnded {
                attempt,
                reason,
                error,
            }) = slot.take_if(|event| matches!(event, SessionEvent::StreamEnded { .. }))
            {
                self.handle_stream_ended(
                    attempt,
                    reason,
                    error,
                    treatment_terminal == Some(attempt),
                );
            }
        }
        for slot in &mut events {
            if matches!(slot, Some(SessionEvent::CleanupBlocked { .. }))
                && let Some(event) = slot.take()
            {
                self.handle_event(event);
            }
        }
        self.defer_effects = false;
        let effect = self.deferred_effect.take();
        let _ = self.drive(effect);
        for slot in &mut events {
            if matches!(slot, Some(SessionEvent::NativeReleased { .. }))
                && let Some(event) = slot.take()
            {
                self.handle_event(event);
            }
        }
        self.reconcile_audio();
        for event in events.into_iter().flatten() {
            match event {
                SessionEvent::OpenVerified { key, receipt } if self.known_key(key) => {
                    if self
                        .model
                        .opening()
                        .is_some_and(|(current, _)| current == key)
                    {
                        ready = Some((key, *receipt));
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
                    diagnostic_failure!(
                        self,
                        Mutation::validation(&self.model, &request),
                        failure,
                        validation_failed(request, failure)
                    );
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
                                diagnostic_failure!(
                                    self,
                                    Mutation::validation(&self.model, &request),
                                    failure,
                                    validation_failed(request, failure)
                                );
                            } else if let Some(accepted) = self.model.accept_prepared(
                                &request,
                                settings,
                                stamp,
                                V::prepared_node(&prepared)
                                    .map(|(node, usb)| (node.to_owned(), usb.to_owned())),
                            ) {
                                match self.runner.prepare_treatments(&accepted.settings) {
                                    Ok(treatments) => {
                                        self.prepared =
                                            Some((accepted.clone(), prepared, treatments));
                                        let effect = diagnostic_model!(
                                            self,
                                            Mutation::ValidationSucceeded,
                                            validation_succeeded(&accepted)
                                        );
                                        let _ = self.drive(effect);
                                    }
                                    Err(failure) => {
                                        if let Some(token) = accepted.choice {
                                            self.validator.retire_selection(token);
                                        }
                                        diagnostic_failure!(
                                            self,
                                            Mutation::validation(&self.model, &accepted),
                                            failure,
                                            validation_failed(accepted, failure)
                                        );
                                    }
                                }
                            } else {
                                let failure = Self::protocol_failure(
                                    request.settings.clone(),
                                    "validation_result",
                                    "prepared settings do not preserve the requested tuple, audio and renderer",
                                );
                                if let Some(token) = request.choice {
                                    self.validator.retire_selection(token);
                                }
                                diagnostic_failure!(
                                    self,
                                    Mutation::validation(&self.model, &request),
                                    failure,
                                    validation_failed(request, failure)
                                );
                            }
                        }
                        ValidationOutcome::SelectionRequired(candidates) => {
                            diagnostic_model!(
                                self,
                                Mutation::Selection,
                                selection_required(&request, candidates)
                            );
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
                            diagnostic_failure!(
                                self,
                                Mutation::validation(&self.model, &completed),
                                failure,
                                validation_failed(completed, failure)
                            );
                        }
                    }
                }
            }
        }
        let _ = self.submit_pending_validation();
        if self.validation.is_none() && self.pending_validation.is_none() {
            let effect = diagnostic_model!(self, Mutation::ContinueRecovery, continue_recovery());
            let _ = self.drive(effect);
        }
        self.reconcile_audio();
        self.finish_drain();
    }
    fn finish_drain(&mut self) {
        self.retire_startup_restore_guard();
        if self
            .model
            .treating()
            .is_none_or(|transition| transition.route() == Some(TreatmentRestoreRoute::FreshOwner))
        {
            self.treatment_apply = None;
        }
        if self
            .verified_applied
            .as_ref()
            .is_some_and(|event| !self.positive_is_valid(event))
        {
            self.verified_applied = None;
        }
        if self
            .admitted_apply
            .is_some_and(|apply| !self.has_user_apply_result_or_pending(apply))
        {
            self.admitted_apply = None;
        }
        let retired = self.validator_shutdown && self.validator.shutdown_complete();
        let validation_drained = self.validation.is_none() && self.pending_validation.is_none();
        diagnostic_model!(
            self,
            Mutation::Drain,
            drain_complete(validation_drained, retired)
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
        failure::{
            Cause, FailureCategory, RendererErrorKind, RendererFailure, RendererOperation, Stage,
            TreatmentConfirmationFailure, TreatmentErrorKind, TreatmentFailure,
            TreatmentFailureDisposition, ValidationLayer,
        },
        render::{RendererProperty, RendererSettings},
        state::{
            AttemptPurpose, CleanupStatus, PauseRequestId, PlaybackState, ProductPhase,
            RendererConfirmation, ValidationKey,
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
            video: crate::domain::capture::VideoRequest::manual(
                crate::domain::capture::ModeRequest {
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
            ),
            audio: AudioSelection::default(),
            renderer: crate::domain::render::RendererSettings::default(),
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
    fn live_treatments(key: AttemptKey, settings: &DraftSettings) -> TreatmentConfirmation {
        let treatment = TreatmentAttemptKey {
            apply: key.apply,
            attempt: key.attempt,
            pass: TreatmentPass::Open,
        };
        TreatmentConfirmation::checked(
            treatment,
            InitialPlayback::Live,
            RendererConfirmation::checked(treatment, settings.renderer.clone()),
        )
        .unwrap()
    }
    fn paused_treatments(key: AttemptKey, settings: &DraftSettings) -> TreatmentConfirmation {
        let treatment = TreatmentAttemptKey {
            apply: key.apply,
            attempt: key.attempt,
            pass: TreatmentPass::Open,
        };
        TreatmentConfirmation::checked(
            treatment,
            InitialPlayback::Paused,
            RendererConfirmation::checked(treatment, settings.renderer.clone()),
        )
        .unwrap()
    }
    fn altered_renderer(base: &RendererSettings) -> RendererSettings {
        let mut values = base.values();
        values.interpolation = !values.interpolation;
        RendererSettings::new(values).unwrap()
    }
    fn receipt(settings: DraftSettings, key: AttemptKey) -> OpenReceipt {
        let treatments = live_treatments(key, &settings);
        OpenReceipt {
            settings,
            verification: VerificationSummary {
                captured_fourcc: FactStatus::Unverified,
                decoded_size: FactStatus::ObservedCompatible,
                nominal_rate: FactStatus::Approximate,
            },
            audio: AudioOutcome::Disabled,
            readiness: OpenReadiness::Live,
            treatments,
        }
    }
    #[derive(Clone)]
    struct TestPrepared {
        settings: DraftSettings,
        stamp: WatchStamp,
        node: Option<(std::path::PathBuf, std::path::PathBuf)>,
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
        native_snapshot: Option<crate::capture::linux::CaptureSnapshot>,
    }
    impl Validator {
        fn finish(&mut self, result: Result<(), ApplyFailure>) {
            let request = self.request.take().unwrap();
            let stamp = self.observed.unwrap_or(request.watch);
            if let Some(snapshot) = &self.native_snapshot {
                let prepared =
                    crate::capture::apply::fixture_request_prepared(&request, snapshot, stamp);
                self.result = Some(ValidationResult {
                    stamp,
                    result: match prepared {
                        Ok(prepared) => {
                            let (node, usb) =
                                crate::capture::apply::CaptureValidator::prepared_node(&prepared)
                                    .expect("native fixture has a physical route");
                            ValidationOutcome::Prepared(TestPrepared {
                                settings: prepared.settings().clone(),
                                stamp,
                                node: Some((node.to_path_buf(), usb.to_path_buf())),
                            })
                        }
                        Err(failure) => ValidationOutcome::Failed(failure),
                    },
                    request,
                });
                return;
            }
            let mut prepared_settings = request.settings.clone();
            if prepared_settings.video.resolved_fourcc.is_none() {
                let crate::domain::capture::InputFormatPolicy::Manual { fourcc } =
                    prepared_settings.video.mode.format_policy
                else {
                    panic!("auto test requires declared native capability evidence");
                };
                prepared_settings.video.resolved_fourcc = Some(fourcc);
            }
            self.result = Some(ValidationResult {
                stamp,
                result: match result {
                    Ok(()) => ValidationOutcome::Prepared(TestPrepared {
                        settings: prepared_settings,
                        stamp,
                        node: None,
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
        fn prepared_node(
            prepared: &Self::Prepared,
        ) -> Option<(&std::path::Path, &std::path::Path)> {
            prepared
                .node
                .as_ref()
                .map(|(node, usb)| (node.as_path(), usb.as_path()))
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
        treatment_intents: Vec<(TreatmentAttemptKey, DraftSettings)>,
        treatment_prepare_failure: Option<ApplyFailure>,
        treatment_preparations: Vec<DraftSettings>,
    }
    impl Runner {
        fn verified(&mut self) {
            let (key, settings, _) = self.opens.last().unwrap();
            let mut receipt = receipt(settings.clone(), *key);
            receipt.readiness = match self.playback.last().unwrap() {
                InitialPlayback::Live => OpenReadiness::Live,
                InitialPlayback::Paused => OpenReadiness::PausedPrepared,
            };
            if receipt.readiness == OpenReadiness::PausedPrepared {
                receipt.treatments = paused_treatments(*key, settings);
            }
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
        type PreparedTreatments = DraftSettings;
        fn prepare_treatments(
            &mut self,
            settings: &DraftSettings,
        ) -> Result<Self::PreparedTreatments, ApplyFailure> {
            self.treatment_preparations.push(settings.clone());
            if let Some(failure) = self.treatment_prepare_failure.take() {
                return Err(failure);
            }
            Ok(settings.clone())
        }
        fn begin_open(
            &mut self,
            key: AttemptKey,
            prepared: TestPrepared,
            treatments: Self::PreparedTreatments,
            gain: PlaybackGain,
            playback: InitialPlayback,
            output: OutputPlan,
        ) -> Result<(), StartFailure> {
            assert_eq!(treatments, prepared.settings);
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
        fn submit_treatments(
            &mut self,
            key: TreatmentAttemptKey,
            treatments: Self::PreparedTreatments,
        ) -> SubmitStatus {
            self.treatment_intents.push((key, treatments));
            self.immediate.take().unwrap_or(SubmitStatus::Accepted)
        }
    }
    fn renderer_settings(variant: u8) -> DraftSettings {
        let mut settings = settings(60);
        let mut values = settings.renderer.values();
        values.scale_antiring = f64::from(variant) / 100.0;
        values.deband.enabled = true;
        values.deband.iterations = 2;
        values.deband.grain = f64::from(variant);
        values.interpolation = true;
        settings.renderer = RendererSettings::new(values).unwrap();
        settings
    }
    #[test]
    fn complete_treatments_survive_each_source_lifecycle_with_only_user_apply_receipts() {
        for route in 0..5 {
            for paused in [false, true] {
                let mut engine = engine();
                let prior = renderer_settings(1);
                engine
                    .edit_draft(engine.model().draft().revision, prior.clone())
                    .unwrap();
                let old = initial(&mut engine);
                let VerifiedApplied::Open { applied, .. } = engine.take_verified_applied().unwrap()
                else {
                    panic!("initial full LIVE proof")
                };
                assert_eq!(applied.settings(), &prior);
                if paused || route == 0 {
                    confirm_pause(&mut engine, old);
                    assert!(
                        engine.runner.treatment_intents.is_empty(),
                        "ordinary pause sends no treatment command"
                    );
                    assert_eq!(engine.runner.treatment_preparations.len(), 1);
                }
                let mut newer = renderer_settings(2);
                newer.video.mode = settings(30).video.mode;
                engine
                    .edit_draft(engine.model().draft().revision, newer.clone())
                    .unwrap();
                let target = if route == 2 {
                    newer.clone()
                } else {
                    prior.clone()
                };
                match route {
                    0 => {
                        engine.resume(engine.model().state_identity(), old).unwrap();
                    }
                    1 => {
                        engine.restart(engine.model().state_identity()).unwrap();
                    }
                    2 => {
                        assert!(matches!(
                            engine
                                .apply(
                                    engine.model().state_identity(),
                                    engine.model().draft().revision
                                )
                                .unwrap(),
                            ApplyAdmission::Open { .. }
                        ));
                    }
                    3 => {
                        engine.close(engine.model().state_identity()).unwrap();
                        engine.runner.barrier(old);
                        engine.poll();
                        assert!(matches!(
                            engine.reconnect(engine.model().state_identity()).unwrap(),
                            ReconnectAdmission::Started(_)
                        ));
                    }
                    4 => {
                        let returned = observation(
                            &engine,
                            2,
                            crate::domain::capture::VideoPresence::Present,
                            SourcePresence::Disabled,
                            Some(2),
                        );
                        observe(&mut engine, returned);
                        assert_eq!(
                            engine.model().recovery().unwrap().applied.settings(),
                            &prior
                        );
                        engine.runner.barrier(old);
                        engine.poll();
                    }
                    _ => unreachable!(),
                }
                assert!(
                    engine
                        .validator
                        .requests
                        .last()
                        .unwrap()
                        .settings
                        .same_request(&target)
                );
                validate(&mut engine);
                if route < 3 {
                    assert_eq!(
                        engine.runner.opens.len(),
                        1,
                        "preparation precedes incumbent retirement"
                    );
                    assert_eq!(engine.runner.treatment_preparations.last(), Some(&target));
                    engine.runner.barrier(old);
                    engine.poll();
                }
                let (key, opened, _) = engine.runner.opens.last().unwrap().clone();
                assert_ne!(key.attempt, old);
                assert_eq!(opened, target);
                assert_eq!(engine.runner.treatment_preparations.last(), Some(&target));
                let prepared_paused = paused && route >= 3;
                assert_eq!(
                    *engine.runner.playback.last().unwrap(),
                    if prepared_paused {
                        InitialPlayback::Paused
                    } else {
                        InitialPlayback::Live
                    }
                );
                assert!(
                    engine.model().active().is_none(),
                    "partial source readiness is not success"
                );
                assert!(engine.take_verified_applied().is_none());
                engine.runner.verified();
                engine.poll();
                assert_eq!(
                    engine.model().active().unwrap().applied().settings(),
                    &target
                );
                assert_eq!(
                    engine.model().active().unwrap().playback(),
                    if prepared_paused {
                        PlaybackState::Paused
                    } else {
                        PlaybackState::Live
                    }
                );
                assert!(engine.model().draft().settings.same_request(&newer));
                assert_eq!(engine.runner.opens.len(), 2);
                if route == 2 {
                    assert!(engine.has_user_apply_result_or_pending(key.apply));
                    assert!(
                        matches!(engine.take_verified_applied(), Some(VerifiedApplied::Open { key: received, applied })
                        if received == key && applied.settings() == &target)
                    );
                } else {
                    assert!(!engine.has_user_apply_result_or_pending(key.apply));
                    assert!(
                        engine.take_verified_applied().is_none(),
                        "lifecycle opens never authorize a user save"
                    );
                }
            }
        }
    }
    #[test]
    fn startup_source_or_catalog_unavailability_preserves_complete_editable_treatments() {
        for catalog in [false, true] {
            let mut engine = engine();
            let submitted = renderer_settings(3);
            engine
                .edit_draft(engine.model().draft().revision, submitted.clone())
                .unwrap();
            let apply = apply(&mut engine);
            engine.require_startup_restore_audio(apply).unwrap();
            let refusal = failure(
                submitted.clone(),
                FailureCategory::Validation(if catalog {
                    ValidationLayer::Renderer
                } else {
                    ValidationLayer::Discovery
                }),
                if catalog {
                    "qualified renderer catalog unavailable"
                } else {
                    "saved capture source unavailable"
                },
            );
            if catalog {
                engine.runner.treatment_prepare_failure = Some(refusal.clone());
                validate(&mut engine);
            } else {
                engine.validator.finish(Err(refusal.clone()));
                engine.poll();
            }
            assert!(engine.model().draft().settings.same_request(&submitted));
            assert_eq!(
                engine.model().validation_rejection().unwrap().failure,
                refusal
            );
            assert!(engine.model().active().is_none());
            assert!(engine.model().last_valid().is_none());
            assert!(engine.runner.opens.is_empty());
            assert!(engine.runner.stops.is_empty());
            assert!(engine.take_verified_applied().is_none());
            assert!(!engine.has_user_apply_result_or_pending(apply));
        }
    }
    fn admit_treatments(engine: &mut Engine, candidate: DraftSettings) -> TreatmentAttemptKey {
        let revision = engine
            .edit_draft(engine.model().draft().revision, candidate)
            .unwrap();
        let ApplyAdmission::Treatments {
            key,
            revision: frozen,
        } = engine
            .apply(engine.model().state_identity(), revision)
            .unwrap()
        else {
            panic!("live renderer admission")
        };
        assert_eq!(frozen, revision);
        key
    }
    fn disposition_for(kind: &TreatmentErrorKind, fresh: bool) -> TreatmentFailureDisposition {
        let rejected = matches!(
            kind,
            TreatmentErrorKind::Renderer(RendererFailure {
                kind: RendererErrorKind::CatalogUnavailable
                    | RendererErrorKind::UnsupportedValue
                    | RendererErrorKind::Submission { .. }
                    | RendererErrorKind::Rejected { .. },
                ..
            })
        );
        if rejected && !fresh {
            TreatmentFailureDisposition::Untouched
        } else if fresh {
            TreatmentFailureDisposition::RestoreFreshOwner
        } else {
            TreatmentFailureDisposition::RestoreSameOwner
        }
    }
    fn treatment_error(
        key: TreatmentAttemptKey,
        kind: TreatmentErrorKind,
        fresh: bool,
    ) -> Box<TreatmentFailure> {
        let disposition = disposition_for(&kind, fresh);
        Box::new(TreatmentFailure {
            key: Some(key),
            kind,
            disposition,
        })
    }
    fn applied_treatment_fault(applied: TreatmentAttemptKey) -> Box<TreatmentFailure> {
        treatment_error(
            applied,
            TreatmentErrorKind::Unconfirmed {
                reason: TreatmentConfirmationFailure::BackendUnavailable,
            },
            true,
        )
    }

    fn renderer_kind(kind: RendererErrorKind) -> TreatmentErrorKind {
        TreatmentErrorKind::Renderer(RendererFailure {
            property: RendererProperty::Scale,
            diagnostic: format!("{kind:?}"),
            kind,
        })
    }
    fn renderer_error(
        key: TreatmentAttemptKey,
        property: RendererProperty,
        kind: RendererErrorKind,
        fresh: bool,
    ) -> Box<TreatmentFailure> {
        Box::new(TreatmentFailure {
            key: Some(key),
            kind: TreatmentErrorKind::Renderer(RendererFailure {
                property,
                kind,
                diagnostic: format!("renderer {} rejected", property.as_str()),
            }),
            disposition: if fresh {
                TreatmentFailureDisposition::RestoreFreshOwner
            } else {
                TreatmentFailureDisposition::Untouched
            },
        })
    }
    fn renderer_property_of(failure: &ApplyFailure) -> Option<RendererProperty> {
        match &failure.treatment.as_deref()?.kind {
            TreatmentErrorKind::Renderer(failure) => Some(failure.property),
            _ => None,
        }
    }
    fn treatment_of(failure: &ApplyFailure) -> Option<&TreatmentFailure> {
        failure.treatment.as_deref()
    }
    fn treatment_success(engine: &mut Engine, key: TreatmentAttemptKey) {
        // Derive the renderer and playback from the frozen candidate/prior.
        // Stale/duplicate success remains well-formed but is refused by identity.
        let (settings, playback) = match engine.model().treating() {
            Some(transition) => {
                let playback = engine
                    .model()
                    .active()
                    .map_or(PlaybackState::Live, |active| active.playback());
                let settings = match key.pass {
                    TreatmentPass::Candidate => transition.candidate().clone(),
                    _ => transition.prior().settings().clone(),
                };
                (settings, playback)
            }
            None => (settings(60), PlaybackState::Live),
        };
        let playback = match playback {
            PlaybackState::Live => InitialPlayback::Live,
            PlaybackState::Paused => InitialPlayback::Paused,
            PlaybackState::PausePending { .. } => panic!("pending playback cannot confirm"),
        };
        let renderer = RendererConfirmation::checked(key, settings.renderer.clone());
        let confirmation = TreatmentConfirmation::checked(key, playback, renderer).unwrap();
        engine
            .runner
            .events
            .push_back(SessionEvent::TreatmentResult {
                key,
                result: Ok(confirmation),
            });
    }
    fn treatment_failed(
        engine: &mut Engine,
        key: TreatmentAttemptKey,
        kind: TreatmentErrorKind,
        fresh: bool,
    ) {
        engine
            .runner
            .events
            .push_back(SessionEvent::TreatmentResult {
                key,
                result: Err(treatment_error(key, kind, fresh)),
            });
    }
    fn deadline() -> TreatmentErrorKind {
        TreatmentErrorKind::Unconfirmed {
            reason: TreatmentConfirmationFailure::Deadline,
        }
    }

    #[test]
    fn correlated_stream_terminal_keeps_exact_candidate_cause_and_frozen_settings_in_both_orders() {
        for ended_first in [false, true] {
            for typed_session in [false, true] {
                let mut engine = engine();
                let old = initial(&mut engine);
                let candidate = renderer_settings(4);
                let key = admit_treatments(&mut engine, candidate.clone());
                if ended_first {
                    engine.runner.events.push_back(SessionEvent::StreamEnded {
                        attempt: old,
                        reason: 4,
                        error: -1,
                    });
                }
                if typed_session {
                    // Gate's original Open snapshot is intentionally different.
                    let terminal = Engine::treatment_failure(
                        settings(60),
                        *treatment_error(
                            key,
                            TreatmentErrorKind::Unconfirmed {
                                reason: TreatmentConfirmationFailure::BackendUnavailable,
                            },
                            true,
                        ),
                    );
                    engine.runner.events.push_back(SessionEvent::SessionFailed {
                        attempt: old,
                        failure: terminal,
                    });
                } else {
                    treatment_failed(
                        &mut engine,
                        key,
                        TreatmentErrorKind::Unconfirmed {
                            reason: TreatmentConfirmationFailure::BackendUnavailable,
                        },
                        true,
                    );
                }
                if !ended_first {
                    engine.runner.events.push_back(SessionEvent::StreamEnded {
                        attempt: old,
                        reason: 4,
                        error: -1,
                    });
                }
                engine.poll();
                assert_eq!(
                    engine.model().treating().unwrap().route(),
                    Some(TreatmentRestoreRoute::FreshOwner)
                );
                assert!(engine.model().recovery().is_none());
                let cause = engine
                    .model()
                    .failures()
                    .unwrap()
                    .candidate
                    .as_ref()
                    .unwrap();
                assert_eq!(cause.requested.as_ref(), &candidate);
                assert_eq!(treatment_of(cause).unwrap().key, Some(key));
                assert_eq!(
                    treatment_of(cause).unwrap().kind,
                    TreatmentErrorKind::Unconfirmed {
                        reason: TreatmentConfirmationFailure::BackendUnavailable
                    }
                );
                engine.runner.barrier(old);
                engine.poll();
                assert_eq!(
                    engine.model().validation_request().unwrap().key.purpose,
                    AttemptPurpose::Restore
                );
                validate(&mut engine);
                engine.runner.verified();
                engine.poll();
                assert_eq!(
                    engine.model().phase(),
                    ProductPhase::ErrorWithActiveRestored
                );
                assert_eq!(engine.runner.opens.len(), 2);
                assert!(engine.take_verified_applied().is_none());
            }
        }
    }
    #[test]
    fn post_transfer_removal_recovers_frozen_prior_after_stale_validation_or_open_and_real_barriers()
     {
        for opening in [false, true] {
            for coalesced_return in [false, true] {
                for paused in [false, true] {
                    let mut engine = engine();
                    let prior = renderer_settings(5);
                    engine
                        .edit_draft(engine.model().draft().revision, prior.clone())
                        .unwrap();
                    let old = initial(&mut engine);
                    engine.take_verified_applied();
                    let apply = if paused {
                        confirm_pause(&mut engine, old);
                        let key = engine.model().confirmed_treatment_key().unwrap();
                        engine
                            .runner
                            .events
                            .push_back(SessionEvent::TreatmentFault {
                                key,
                                failure: treatment_error(
                                    key,
                                    TreatmentErrorKind::Unconfirmed {
                                        reason: TreatmentConfirmationFailure::BackendUnavailable,
                                    },
                                    true,
                                ),
                            });
                        engine.poll();
                        engine.model().state_identity().operation().unwrap()
                    } else {
                        let key = admit_treatments(&mut engine, renderer_settings(6));
                        treatment_failed(
                            &mut engine,
                            key,
                            TreatmentErrorKind::Unconfirmed {
                                reason: TreatmentConfirmationFailure::BackendUnavailable,
                            },
                            true,
                        );
                        engine.poll();
                        key.apply
                    };
                    let candidate_cause = engine
                        .model()
                        .failures()
                        .unwrap()
                        .candidate
                        .clone()
                        .unwrap();
                    engine.runner.barrier(old);
                    engine.poll();
                    assert_eq!(engine.model().phase(), ProductPhase::ValidatingPrior);
                    let restore_request = engine.model().validation_request().unwrap().clone();
                    let expected_playback = if paused {
                        InitialPlayback::Paused
                    } else {
                        InitialPlayback::Live
                    };
                    assert_eq!(restore_request.playback, expected_playback);
                    let mut restore_owner = None;
                    if opening {
                        validate(&mut engine);
                        restore_owner = Some(engine.model().opening().unwrap().0.attempt);
                        engine.runner.verified(); // Cached readiness must lose to removal.
                    }
                    let newer = renderer_settings(7);
                    engine
                        .edit_draft(engine.model().draft().revision, newer.clone())
                        .unwrap();
                    let removal = observation(
                        &engine,
                        2,
                        if coalesced_return {
                            crate::domain::capture::VideoPresence::Present
                        } else {
                            crate::domain::capture::VideoPresence::Absent
                        },
                        SourcePresence::Disabled,
                        Some(2),
                    );
                    observe(&mut engine, removal);
                    assert_eq!(
                        engine.model().recovery().unwrap().applied.settings(),
                        &prior
                    );
                    assert_eq!(
                        engine.model().recovery().unwrap().playback,
                        expected_playback
                    );
                    assert_eq!(
                        engine.model().failures().unwrap().candidate.as_ref(),
                        Some(&candidate_cause)
                    );
                    assert!(engine.model().active().is_none());
                    assert!(!engine.has_user_apply_result_or_pending(apply));
                    assert!(engine.take_verified_applied().is_none());
                    if !opening {
                        engine.validator.finish(Ok(())); // Real accepted stale Restore result drains.
                        engine.poll();
                        assert_eq!(engine.runner.opens.len(), 1);
                    }
                    if !coalesced_return {
                        let returned = observation(
                            &engine,
                            3,
                            crate::domain::capture::VideoPresence::Present,
                            SourcePresence::Disabled,
                            Some(2),
                        );
                        observe(&mut engine, returned);
                    }
                    if let Some(attempt) = restore_owner {
                        assert_eq!(engine.model().cleanup(), &CleanupStatus::Draining);
                        assert!(engine.model().validation_request().is_none());
                        engine
                            .runner
                            .events
                            .push_back(SessionEvent::NativeReleased { attempt });
                        engine.poll();
                        assert!(
                            engine.model().validation_request().is_none(),
                            "release before owner destruction is not recovery authorization"
                        );
                        engine.runner.barrier(attempt);
                        engine.poll();
                    }
                    let recovery = engine.model().validation_request().unwrap();
                    assert_eq!(recovery.key.purpose, AttemptPurpose::Recovery);
                    assert_eq!(recovery.settings, prior);
                    assert_eq!(recovery.playback, expected_playback);
                    validate(&mut engine);
                    engine.runner.verified();
                    engine.poll();
                    assert_eq!(
                        engine.model().active().unwrap().applied().settings(),
                        &prior
                    );
                    assert_eq!(
                        engine.model().active().unwrap().playback(),
                        if paused {
                            PlaybackState::Paused
                        } else {
                            PlaybackState::Live
                        }
                    );
                    assert!(engine.model().draft().settings.same_request(&newer));
                    assert_eq!(
                        engine.model().failures().unwrap().candidate.as_ref(),
                        Some(&candidate_cause)
                    );
                    assert_eq!(
                        engine
                            .validator
                            .requests
                            .iter()
                            .filter(|request| request.key.purpose == AttemptPurpose::Restore)
                            .count(),
                        1
                    );
                    assert_eq!(engine.runner.opens.len(), if opening { 3 } else { 2 });
                    assert!(!engine.has_user_apply_result_or_pending(apply));
                }
            }
        }
    }
    #[test]
    fn late_confirmed_fault_supersedes_source_restart_and_resume_validation_or_closing_in_both_drain_orders()
     {
        for operation in 0..3 {
            for closing in [false, true] {
                for barrier_first in [false, true] {
                    for restore_fails in [false, true] {
                        let mut engine = engine();
                        let prior = renderer_settings(8);
                        engine
                            .edit_draft(engine.model().draft().revision, prior.clone())
                            .unwrap();
                        let old = initial(&mut engine);
                        engine.take_verified_applied();
                        if operation == 2 {
                            confirm_pause(&mut engine, old);
                        }
                        let confirmed = engine.model().confirmed_treatment_key().unwrap();
                        let superseded = match operation {
                            0 => {
                                let mut changed = settings(30);
                                changed.renderer = renderer_settings(9).renderer;
                                engine
                                    .edit_draft(engine.model().draft().revision, changed)
                                    .unwrap();
                                engine
                                    .apply(
                                        engine.model().state_identity(),
                                        engine.model().draft().revision,
                                    )
                                    .unwrap()
                                    .id()
                            }
                            1 => engine.restart(engine.model().state_identity()).unwrap(),
                            2 => engine.resume(engine.model().state_identity(), old).unwrap(),
                            _ => unreachable!(),
                        };
                        let request = engine.model().validation_request().unwrap().clone();
                        if closing {
                            validate(&mut engine);
                        }
                        let newer = renderer_settings(10);
                        engine
                            .edit_draft(engine.model().draft().revision, newer.clone())
                            .unwrap();
                        engine
                            .runner
                            .events
                            .push_back(SessionEvent::TreatmentFault {
                                key: confirmed,
                                failure: treatment_error(
                                    confirmed,
                                    TreatmentErrorKind::Unconfirmed {
                                        reason: TreatmentConfirmationFailure::BackendUnavailable,
                                    },
                                    true,
                                ),
                            });
                        engine.poll();
                        assert_eq!(engine.model().phase(), ProductPhase::RestoringTreatments);
                        assert_eq!(
                            engine.model().treating().unwrap().route(),
                            Some(TreatmentRestoreRoute::FreshOwner)
                        );
                        assert_eq!(
                            engine.model().treating().unwrap().prior().settings(),
                            &prior
                        );
                        assert!(engine.prepared.is_none() && engine.pending_validation.is_none());
                        assert!(!engine.has_user_apply_result_or_pending(superseded));
                        if !closing {
                            assert!(engine.validator.cancelled);
                        }
                        let cause = engine
                            .model()
                            .failures()
                            .unwrap()
                            .candidate
                            .clone()
                            .unwrap();
                        assert_eq!(cause.requested.as_ref(), &prior);
                        assert_eq!(treatment_of(&cause).unwrap().key, Some(confirmed));
                        if barrier_first {
                            engine.runner.barrier(old);
                            engine.poll();
                        }
                        if !closing {
                            engine.validator.finish(Ok(()));
                        } else {
                            // An old completed result cannot replace the new Restore preparation.
                            engine.validator.result = Some(ValidationResult {
                                stamp: request.watch,
                                result: ValidationOutcome::Prepared(TestPrepared {
                                    settings: request.settings.clone(),
                                    stamp: request.watch,
                                    node: None,
                                }),
                                request: request.clone(),
                            });
                        }
                        engine.poll();
                        assert_eq!(
                            engine.runner.opens.len(),
                            1,
                            "superseded source candidate must never open"
                        );
                        if !barrier_first {
                            engine.runner.barrier(old);
                            engine.poll();
                        }
                        let restore = engine.model().validation_request().unwrap();
                        assert_eq!(restore.key.purpose, AttemptPurpose::Restore);
                        assert_eq!(restore.settings, prior);
                        assert_eq!(
                            restore.playback,
                            if operation == 2 {
                                InitialPlayback::Paused
                            } else {
                                InitialPlayback::Live
                            }
                        );
                        validate(&mut engine);
                        let fresh = engine.model().opening().unwrap().0;
                        if restore_fails {
                            engine.runner.fail("single fresh restore failed");
                            engine.poll();
                            engine.runner.barrier(fresh.attempt);
                            engine.poll();
                            assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
                            assert!(engine.model().failures().unwrap().restore.is_some());
                            assert!(engine.model().validation_request().is_none());
                        } else {
                            engine.runner.verified();
                            engine.poll();
                            assert_eq!(
                                engine.model().phase(),
                                ProductPhase::ErrorWithActiveRestored
                            );
                            assert_eq!(
                                engine.model().active().unwrap().applied().settings(),
                                &prior
                            );
                        }
                        assert!(engine.model().draft().settings.same_request(&newer));
                        assert_eq!(
                            engine.model().failures().unwrap().candidate.as_ref(),
                            Some(&cause)
                        );
                        assert_eq!(engine.runner.opens.len(), 2);
                        assert_eq!(
                            engine
                                .validator
                                .requests
                                .iter()
                                .filter(|request| request.key.purpose == AttemptPurpose::Restore)
                                .count(),
                            1
                        );
                        assert!(engine.take_verified_applied().is_none());
                        assert!(!engine.has_user_apply_result_or_pending(superseded));
                    }
                }
            }
        }
    }

    #[test]
    fn matching_removal_outranks_old_treatment_fault_during_source_restart_and_resume_validation() {
        fn unchanged(actual: &Engine, ordinary: &Engine, admitted: ApplyId) {
            assert_eq!(
                actual.model().state_identity(),
                ordinary.model().state_identity()
            );
            assert!(actual.model().treating().is_none());
            assert_eq!(actual.model().active(), ordinary.model().active());
            assert_eq!(actual.model().last_valid(), ordinary.model().last_valid());
            assert_eq!(actual.model().draft(), ordinary.model().draft());
            assert_eq!(
                actual.model().validation_request(),
                ordinary.model().validation_request()
            );
            assert_eq!(actual.model().opening(), ordinary.model().opening());
            assert_eq!(
                actual.model().can_reconnect(),
                ordinary.model().can_reconnect()
            );
            assert_eq!(
                actual.has_user_apply_result_or_pending(admitted),
                ordinary.has_user_apply_result_or_pending(admitted)
            );
            let mut loss = actual.model().recovery().cloned();
            if let Some(loss) = &mut loss {
                loss.failure.treatment = None;
            }
            assert_eq!(loss.as_ref(), ordinary.model().recovery());
            let mut report = actual.model().failures().cloned();
            if let Some(incumbent) = report.as_mut().and_then(|report| report.incumbent.as_mut()) {
                incumbent.treatment = None;
            }
            assert_eq!(report.as_ref(), ordinary.model().failures());
            assert_eq!(actual.validator.cancelled, ordinary.validator.cancelled);
            assert_eq!(actual.validator.requests, ordinary.validator.requests);
            assert_eq!(actual.runner.opens, ordinary.runner.opens);
            assert_eq!(actual.runner.stops, ordinary.runner.stops);
            assert_eq!(
                actual.runner.treatment_preparations,
                ordinary.runner.treatment_preparations
            );
            assert!(
                actual.runner.treatment_intents.is_empty(),
                "source-loss evidence cannot submit treatment work"
            );
        }
        for operation in 0..3 {
            for fault_timing in 0..4 {
                for typed_terminal in [false, true] {
                    for coalesced_return in [false, true] {
                        for validation_before_release in [false, true] {
                            if fault_timing == 3 && validation_before_release {
                                continue;
                            }
                            let prior = renderer_settings(11);
                            let newer = renderer_settings(12);
                            let mut actual = engine();
                            let mut ordinary = engine();
                            for engine in [&mut actual, &mut ordinary] {
                                engine
                                    .edit_draft(engine.model().draft().revision, prior.clone())
                                    .unwrap();
                                let old = initial(engine);
                                engine
                                    .take_verified_applied()
                                    .expect("initial candidate receipt");
                                if operation == 2 {
                                    confirm_pause(engine, old);
                                }
                                match operation {
                                    0 => {
                                        let mut source = settings(30);
                                        source.renderer = renderer_settings(13).renderer;
                                        engine
                                            .edit_draft(engine.model().draft().revision, source)
                                            .unwrap();
                                        assert!(matches!(
                                            engine
                                                .apply(
                                                    engine.model().state_identity(),
                                                    engine.model().draft().revision
                                                )
                                                .unwrap(),
                                            ApplyAdmission::Open { .. }
                                        ));
                                    }
                                    1 => {
                                        engine.restart(engine.model().state_identity()).unwrap();
                                    }
                                    2 => {
                                        engine
                                            .resume(engine.model().state_identity(), old)
                                            .unwrap();
                                    }
                                    _ => unreachable!(),
                                }
                                engine
                                    .edit_draft(engine.model().draft().revision, newer.clone())
                                    .unwrap();
                            }
                            let key = actual.model().confirmed_treatment_key().unwrap();
                            let old = key.attempt;
                            let request = actual.model().validation_request().unwrap().clone();
                            let admitted = request.key.apply;
                            let kind = if typed_terminal {
                                TreatmentErrorKind::Unconfirmed {
                                    reason: TreatmentConfirmationFailure::EvidenceLost,
                                }
                            } else {
                                TreatmentErrorKind::Unconfirmed {
                                    reason: TreatmentConfirmationFailure::BackendUnavailable,
                                }
                            };
                            let fault = if typed_terminal {
                                SessionEvent::SessionFailed {
                                    attempt: old,
                                    failure: Engine::treatment_failure(
                                        prior.clone(),
                                        *treatment_error(key, kind.clone(), true),
                                    ),
                                }
                            } else {
                                SessionEvent::TreatmentFault {
                                    key,
                                    failure: treatment_error(key, kind.clone(), true),
                                }
                            };
                            if fault_timing == 0 {
                                actual.runner.events.push_back(fault.clone());
                            }
                            for engine in [&mut actual, &mut ordinary] {
                                let removal = observation(
                                    engine,
                                    2,
                                    if coalesced_return {
                                        crate::domain::capture::VideoPresence::Present
                                    } else {
                                        crate::domain::capture::VideoPresence::Absent
                                    },
                                    SourcePresence::Disabled,
                                    Some(2),
                                );
                                observe(engine, removal);
                            }
                            assert!(
                                !actual.current_treatment_key(key),
                                "lost proof is diagnostic-only, never treatment terminal correlation"
                            );
                            if fault_timing == 2 {
                                for engine in [&mut actual, &mut ordinary] {
                                    engine.runner.events.push_back(SessionEvent::OwnerStopped {
                                        attempt: old,
                                        outcome: Ok(()),
                                    });
                                    engine.poll();
                                }
                            } else if fault_timing == 3 {
                                for engine in [&mut actual, &mut ordinary] {
                                    engine.runner.barrier(old);
                                    engine.poll();
                                }
                            }
                            if fault_timing != 0 {
                                actual.runner.events.push_back(fault);
                            }
                            for engine in [&mut actual, &mut ordinary] {
                                engine.runner.events.push_back(SessionEvent::StreamEnded {
                                    attempt: old,
                                    reason: 4,
                                    error: -1,
                                });
                                engine.poll();
                            }
                            unchanged(&actual, &ordinary, admitted);
                            let loss = actual.model().recovery().unwrap();
                            assert_eq!(loss.applied.settings(), &prior);
                            assert_eq!(
                                loss.playback,
                                if operation == 2 {
                                    InitialPlayback::Paused
                                } else {
                                    InitialPlayback::Live
                                }
                            );
                            assert!(matches!(loss.evidence, LossEvidence::Removed { .. }));
                            let retained = treatment_of(&loss.failure);
                            if fault_timing == 3 {
                                assert!(retained.is_none(), "retired-owner evidence is stale");
                            } else {
                                let retained = retained.expect(
                                    "exact keyed native evidence retained alongside removal",
                                );
                                assert_eq!(retained.key, Some(key));
                                assert_eq!(retained.kind, kind);
                                assert_eq!(
                                    treatment_of(
                                        actual
                                            .model()
                                            .failures()
                                            .unwrap()
                                            .incumbent
                                            .as_ref()
                                            .unwrap()
                                    ),
                                    Some(retained)
                                );
                            }
                            for engine in [&mut actual, &mut ordinary] {
                                if !validation_before_release && fault_timing != 3 {
                                    engine.runner.barrier(old);
                                    engine.poll();
                                }
                                engine.validator.finish(Err(failure(
                                    request.settings.clone(),
                                    FailureCategory::Validation(ValidationLayer::Mode),
                                    "source unavailable during validation",
                                )));
                                engine.poll();
                                if validation_before_release {
                                    let state = engine.model().state_identity();
                                    assert_eq!(
                                        engine.reconnect(state).unwrap(),
                                        ReconnectAdmission::Joined(state.operation().unwrap()),
                                        "Reconnect joins frozen recovery without authorizing an early owner",
                                    );
                                    assert_eq!(
                                        engine.runner.opens.len(),
                                        1,
                                        "no owner opens before the native barrier"
                                    );
                                    assert!(engine.model().opening().is_none());
                                    engine.runner.barrier(old);
                                    engine.poll();
                                }
                            }
                            unchanged(&actual, &ordinary, admitted);
                            assert!(!actual.has_user_apply_result_or_pending(admitted));
                            assert!(actual.take_verified_applied().is_none());
                            assert_eq!(actual.runner.opens.len(), 1);
                            assert!(
                                actual
                                    .validator
                                    .requests
                                    .iter()
                                    .all(|request| request.key.purpose != AttemptPurpose::Restore)
                            );
                            for engine in [&mut actual, &mut ordinary] {
                                let returned = observation(
                                    engine,
                                    3,
                                    crate::domain::capture::VideoPresence::Present,
                                    SourcePresence::Disabled,
                                    Some(2),
                                );
                                observe(engine, returned);
                                let recovery = engine.model().validation_request().unwrap();
                                assert_eq!(recovery.key.purpose, AttemptPurpose::Recovery);
                                assert_eq!(recovery.settings, prior);
                                assert_eq!(
                                    recovery.playback,
                                    if operation == 2 {
                                        InitialPlayback::Paused
                                    } else {
                                        InitialPlayback::Live
                                    }
                                );
                                validate(engine);
                                engine.runner.verified();
                                engine.poll();
                            }
                            unchanged(&actual, &ordinary, admitted);
                            assert_eq!(
                                actual.model().active().unwrap().applied().settings(),
                                &prior
                            );
                            assert!(actual.model().draft().settings.same_request(&newer));
                            assert_eq!(
                                actual.runner.opens.len(),
                                2,
                                "only positive return may authorize ordinary Recovery"
                            );
                            assert!(
                                actual
                                    .validator
                                    .requests
                                    .iter()
                                    .all(|request| request.key.purpose != AttemptPurpose::Restore)
                            );
                            assert!(!actual.has_user_apply_result_or_pending(admitted));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn live_apply_emits_only_exact_frozen_complete_renderer_result() {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine.take_verified_applied();
        let candidate = renderer_settings(14);
        let key = admit_treatments(&mut engine, candidate.clone());
        let revision = engine.model().draft().revision;
        let admitted_state = engine.model().state_identity();
        assert_eq!(
            engine.apply(admitted_state, DraftRevision::new(revision.get() + 1)),
            Err(CommandRejection::ApplyInProgress)
        );
        assert!(engine.take_verified_applied().is_none());
        assert_eq!(engine.runner.treatment_preparations.len(), 3);
        assert_eq!(engine.runner.treatment_preparations[1], candidate);
        assert_eq!(engine.runner.treatment_preparations[2], settings(60));
        assert_eq!(
            engine.runner.treatment_intents,
            vec![(key, candidate.clone())]
        );
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
        assert!(engine.has_user_apply_result_or_pending(key.apply));
        engine
            .edit_draft(engine.model().draft().revision, renderer_settings(15))
            .unwrap();
        assert_eq!(
            engine.set_gain(PlaybackGain::new(37, false).unwrap()),
            SubmitStatus::Accepted
        );
        treatment_success(&mut engine, key);
        engine.poll();
        assert_eq!(engine.model().last_valid().unwrap().settings(), &candidate);
        assert!(
            engine
                .model()
                .draft()
                .settings
                .same_request(&renderer_settings(15))
        );
        assert_eq!(engine.model().active().unwrap().attempt(), old);
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
        assert!(engine.has_user_apply_result_or_pending(key.apply));
        let VerifiedApplied::Treatments {
            key: received,
            revision: frozen,
            applied,
        } = engine
            .take_verified_applied()
            .expect("exact candidate renderer receipt")
        else {
            panic!("renderer receipt must never masquerade as Open")
        };
        assert_eq!(received, key);
        assert_eq!(frozen, revision);
        assert_eq!(applied.settings(), &candidate);
        assert!(!engine.has_user_apply_result_or_pending(key.apply));
        treatment_success(&mut engine, key);
        engine.poll();
        assert_eq!(engine.model().last_valid().unwrap().settings(), &candidate);
        assert!(
            engine.take_verified_applied().is_none(),
            "duplicates cannot replace the consumed slot"
        );
    }

    #[test]
    fn renderer_only_live_treatment_keeps_owner_attempt_without_open() {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine.take_verified_applied();
        let mut candidate = engine.model().draft().settings.clone();
        candidate.renderer = altered_renderer(&candidate.renderer);
        let revision = engine
            .edit_draft(engine.model().draft().revision, candidate.clone())
            .unwrap();
        let ApplyAdmission::Treatments {
            key,
            revision: frozen,
        } = engine
            .apply(engine.model().state_identity(), revision)
            .unwrap()
        else {
            panic!("renderer-only live apply must use the hot treatment route")
        };
        assert_eq!(frozen, revision);
        assert_eq!(key.pass, TreatmentPass::Candidate);
        assert_eq!(key.attempt, old, "the same owner executes the hot route");
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
        assert_eq!(
            engine.runner.treatment_intents,
            vec![(key, candidate.clone())],
            "the submitted bundle carries the requested renderer"
        );
        assert_eq!(
            engine.model().active().unwrap().attempt(),
            old,
            "the incumbent remains the last-verified configuration while treating"
        );
        treatment_success(&mut engine, key);
        engine.poll();
        assert_eq!(engine.model().active().unwrap().attempt(), old);
        assert_eq!(
            engine
                .model()
                .active()
                .unwrap()
                .applied()
                .settings()
                .renderer,
            candidate.renderer
        );
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
        let VerifiedApplied::Treatments {
            key: received,
            applied,
            ..
        } = engine
            .take_verified_applied()
            .expect("renderer-only receipt")
        else {
            panic!("renderer-only receipt is a treatment, never an open")
        };
        assert_eq!(received, key);
        assert_eq!(applied.settings().renderer, candidate.renderer);
    }

    #[test]
    fn renderer_only_paused_treatment_preserves_pause_and_never_opens() {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine.take_verified_applied();
        confirm_pause(&mut engine, old);
        let mut candidate = engine.model().draft().settings.clone();
        candidate.renderer = altered_renderer(&candidate.renderer);
        let revision = engine
            .edit_draft(engine.model().draft().revision, candidate.clone())
            .unwrap();
        let ApplyAdmission::Treatments { key, .. } = engine
            .apply(engine.model().state_identity(), revision)
            .unwrap()
        else {
            panic!("renderer-only paused apply must keep the hot treatment route")
        };
        assert_eq!(key.attempt, old);
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
        treatment_success(&mut engine, key);
        engine.poll();
        let active = engine.model().active().unwrap();
        assert_eq!(active.attempt(), old);
        assert_eq!(active.playback(), PlaybackState::Paused);
        assert_eq!(active.applied().settings().renderer, candidate.renderer);
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
    }

    #[test]
    fn paused_renderer_change_keeps_owner_without_unpause_or_open() {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine.take_verified_applied();
        confirm_pause(&mut engine, old);
        let mut candidate = engine.model().draft().settings.clone();
        candidate.renderer = renderer_settings(16).renderer;
        let revision = engine
            .edit_draft(engine.model().draft().revision, candidate)
            .unwrap();
        assert!(matches!(
            engine
                .apply(engine.model().state_identity(), revision)
                .unwrap(),
            ApplyAdmission::Treatments { .. }
        ));
        assert_eq!(engine.runner.treatment_intents.len(), 1);
        assert!(engine.runner.stops.is_empty());
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.intents.iter().all(|(_, intent)| !matches!(
            intent,
            ImmediateIntent::SetPaused { paused: false, .. }
        )));
    }

    #[test]
    fn mutated_renderer_candidate_restores_prior_bundle_once() {
        let mut engine = engine();
        initial(&mut engine);
        engine.take_verified_applied();
        let mut candidate = engine.model().draft().settings.clone();
        candidate.renderer = renderer_settings(17).renderer;
        let revision = engine
            .edit_draft(engine.model().draft().revision, candidate.clone())
            .unwrap();
        let ApplyAdmission::Treatments { key, .. } = engine
            .apply(engine.model().state_identity(), revision)
            .unwrap()
        else {
            panic!("live renderer apply uses the hot treatment route")
        };
        assert_eq!(engine.runner.treatment_intents[0], (key, candidate.clone()));
        treatment_failed(
            &mut engine,
            key,
            renderer_kind(RendererErrorKind::ReadbackMismatch),
            false,
        );
        engine.poll();
        let restore = engine.model().treating().unwrap().key();
        assert_eq!(restore.pass, TreatmentPass::Restore);
        assert_eq!(
            engine.runner.treatment_intents.len(),
            2,
            "exactly one same-owner restore of the frozen prior"
        );
        assert_eq!(engine.runner.treatment_intents[1].1, settings(60));
        assert!(engine.runner.stops.is_empty());
        treatment_success(&mut engine, restore);
        engine.poll();
        assert_eq!(
            engine.model().phase(),
            ProductPhase::ErrorWithActiveRestored
        );
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.take_verified_applied().is_none());
    }

    #[test]
    fn paused_renderer_failures_preserve_or_restore_frozen_prior_without_unpause_or_save() {
        for route in 0..3 {
            let mut engine = engine();
            let old = initial(&mut engine);
            engine.take_verified_applied();
            confirm_pause(&mut engine, old);
            let prior = engine.model().last_valid().unwrap().settings().clone();
            let key = admit_treatments(&mut engine, renderer_settings(41));
            let newer = renderer_settings(42);
            engine
                .edit_draft(engine.model().draft().revision, newer.clone())
                .unwrap();
            let kind = match route {
                0 => renderer_kind(RendererErrorKind::Rejected { mpv_error: -5 }),
                1 => renderer_kind(RendererErrorKind::ReadbackMismatch),
                2 => TreatmentErrorKind::Unconfirmed {
                    reason: TreatmentConfirmationFailure::EvidenceLost,
                },
                _ => unreachable!(),
            };
            treatment_failed(&mut engine, key, kind, route == 2);
            engine.poll();
            match route {
                0 => {
                    assert!(engine.model().treating().is_none());
                    assert_eq!(engine.runner.treatment_intents.len(), 1);
                    assert!(engine.runner.stops.is_empty());
                }
                1 => {
                    let transition = engine.model().treating().unwrap();
                    assert_eq!(transition.route(), Some(TreatmentRestoreRoute::SameOwner));
                    let restore = transition.key();
                    assert_eq!(restore.pass, TreatmentPass::Restore);
                    assert_eq!(restore.attempt, old);
                    assert_eq!(engine.runner.treatment_intents.len(), 2);
                    assert_eq!(engine.runner.treatment_intents[1].1, prior);
                    treatment_success(&mut engine, restore);
                    engine.poll();
                    assert!(engine.runner.stops.is_empty());
                }
                2 => {
                    assert_eq!(
                        engine.model().treating().unwrap().route(),
                        Some(TreatmentRestoreRoute::FreshOwner)
                    );
                    assert_eq!(engine.runner.stops.len(), 1);
                    engine.runner.events.push_back(SessionEvent::OwnerStopped {
                        attempt: old,
                        outcome: Ok(()),
                    });
                    engine.poll();
                    assert!(engine.model().validation_request().is_none());
                    assert_eq!(engine.runner.opens.len(), 1);
                    engine
                        .runner
                        .events
                        .push_back(SessionEvent::NativeReleased { attempt: old });
                    engine.poll();
                    let request = engine.model().validation_request().unwrap();
                    assert_eq!(request.settings, prior);
                    assert_eq!(request.playback, InitialPlayback::Paused);
                    validate(&mut engine);
                    engine.runner.verified();
                    engine.poll();
                    assert_eq!(engine.runner.opens.len(), 2);
                }
                _ => unreachable!(),
            }
            assert_eq!(
                engine.model().phase(),
                ProductPhase::ErrorWithActiveRestored
            );
            let active = engine.model().active().unwrap();
            assert_eq!(active.playback(), PlaybackState::Paused);
            assert_eq!(active.applied().settings(), &prior);
            assert!(engine.model().draft().settings.same_request(&newer));
            assert!(engine.take_verified_applied().is_none());
            assert!(!engine.has_user_apply_result_or_pending(key.apply));
            assert!(engine.runner.intents.iter().all(|(_, intent)| !matches!(
                intent,
                ImmediateIntent::SetPaused { paused: false, .. }
            )));
            if route != 2 {
                assert_eq!(engine.runner.opens.len(), 1);
            }
        }
    }

    #[test]
    fn late_native_fault_uses_latest_renderer_key_and_stale_proof_is_ignored() {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine.take_verified_applied();
        let previous = engine.model().confirmed_treatment_key().unwrap();
        assert_eq!(previous.attempt, old);
        let mut candidate = engine.model().draft().settings.clone();
        candidate.renderer = altered_renderer(&candidate.renderer);
        let revision = engine
            .edit_draft(engine.model().draft().revision, candidate)
            .unwrap();
        let ApplyAdmission::Treatments { key, .. } = engine
            .apply(engine.model().state_identity(), revision)
            .unwrap()
        else {
            panic!("renderer-only hot route")
        };
        treatment_success(&mut engine, key);
        engine.poll();
        assert!(engine.model().active().is_some());
        let applied = engine.model().last_valid().unwrap().settings().clone();
        assert_eq!(engine.model().confirmed_treatment_key(), Some(key));
        assert_ne!(previous, key);
        engine
            .runner
            .events
            .push_back(SessionEvent::TreatmentFault {
                key: previous,
                failure: applied_treatment_fault(previous),
            });
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Active);
        assert!(engine.runner.stops.is_empty());
        // Only a fault keyed to the most recent renderer proof revokes it.
        engine
            .runner
            .events
            .push_back(SessionEvent::TreatmentFault {
                key,
                failure: applied_treatment_fault(key),
            });
        engine.poll();
        // active() deliberately reports the frozen last-verified selection
        // while Treating rather than provisional native health, so the
        // consumer-visible revocation is the model leaving Active for exactly
        // one fresh-owner restoration of the whole frozen prior, with no
        // reusable receipt and no new open before the owner barrier.
        let restore = engine
            .model()
            .treating()
            .expect("the latest renderer fault selects exactly one restoration");
        assert!(!engine.has_user_apply_result_or_pending(key.apply));
        assert_eq!(engine.model().phase(), ProductPhase::RestoringTreatments);
        assert_eq!(restore.route(), Some(TreatmentRestoreRoute::FreshOwner));
        assert_eq!(restore.key().pass, TreatmentPass::Restore);
        assert_eq!(restore.prior().settings(), &applied);
        assert_eq!(engine.runner.stops.len(), 1);
        assert!(engine.take_verified_applied().is_none());
        assert_eq!(engine.runner.opens.len(), 1);
    }

    #[test]
    fn same_drain_renderer_rejection_and_incumbent_fault_restore_current_prior_once() {
        for fault_first in [false, true] {
            let mut engine = engine();
            let old = initial(&mut engine);
            engine.take_verified_applied();
            // A hot commit advances the exact incumbent renderer proof key.
            let mut renderer_only = engine.model().draft().settings.clone();
            renderer_only.renderer = altered_renderer(&renderer_only.renderer);
            let renderer_key = admit_treatments(&mut engine, renderer_only.clone());
            treatment_success(&mut engine, renderer_key);
            engine.poll();
            let applied = engine.model().confirmed_treatment_key().unwrap();
            assert_eq!(applied, renderer_key);
            assert_eq!(applied.attempt, old);

            // Candidate B is a new renderer-only attempt whose first SET is
            // definitively rejected without a prior mutation.
            let mut candidate = renderer_only.clone();
            candidate.renderer = altered_renderer(&candidate.renderer);
            let key = admit_treatments(&mut engine, candidate);
            let rejection = SessionEvent::TreatmentResult {
                key,
                result: Err(renderer_error(
                    key,
                    RendererProperty::Scale,
                    RendererErrorKind::Rejected { mpv_error: -5 },
                    false,
                )),
            };
            // An incumbent native proof fault arrives in the same drain.
            let origin_fault = SessionEvent::TreatmentFault {
                key: applied,
                failure: applied_treatment_fault(applied),
            };
            if fault_first {
                engine.runner.events.push_back(origin_fault);
                engine.runner.events.push_back(rejection);
            } else {
                engine.runner.events.push_back(rejection);
                engine.runner.events.push_back(origin_fault);
            }
            engine.poll();
            assert_eq!(
                engine.model().treating().unwrap().route(),
                Some(TreatmentRestoreRoute::FreshOwner)
            );
            assert_eq!(engine.runner.stops.len(), 1, "exactly one fresh route");
            assert_eq!(engine.runner.treatment_intents.len(), 2);
            let failures = engine.model().failures().unwrap();
            // The candidate keeps its typed renderer diagnostic as the primary
            // failure rather than being overwritten by the incumbent fault.
            assert_eq!(
                renderer_property_of(failures.candidate.as_ref().unwrap()),
                Some(RendererProperty::Scale)
            );
            let incumbent = failures.incumbent.as_ref().unwrap();
            assert_eq!(
                incumbent.treatment.as_deref().unwrap().key,
                Some(applied),
                "the incumbent fault retains the latest applied renderer key"
            );
            let native = treatment_of(incumbent).expect("native proof fault");
            assert_eq!(
                native.kind,
                TreatmentErrorKind::Unconfirmed {
                    reason: TreatmentConfirmationFailure::BackendUnavailable
                }
            );
            assert_eq!(native.key, Some(applied));

            engine.runner.barrier(old);
            engine.poll();
            assert_eq!(
                engine.model().validation_request().unwrap().key.purpose,
                AttemptPurpose::Restore
            );
            assert_eq!(
                engine.model().validation_request().unwrap().settings,
                renderer_only
            );
            validate(&mut engine);
            engine.runner.verified();
            engine.poll();
            assert_eq!(
                engine.model().phase(),
                ProductPhase::ErrorWithActiveRestored
            );
            assert_eq!(
                engine.model().last_valid().unwrap().settings(),
                &renderer_only
            );
            assert_eq!(
                engine
                    .model()
                    .confirmed_treatments()
                    .unwrap()
                    .renderer()
                    .settings(),
                &renderer_only.renderer
            );
            assert_eq!(
                engine.model().confirmed_treatment_key().unwrap().pass,
                TreatmentPass::Open
            );
            assert_eq!(engine.runner.opens.len(), 2);
        }
    }

    #[test]
    fn late_native_fault_after_untouched_rejection_preserves_primary_and_restores_prior_once() {
        for restore_ok in [true, false] {
            let mut engine = engine();
            let old = initial(&mut engine);
            engine.take_verified_applied();
            let mut renderer_only = engine.model().draft().settings.clone();
            renderer_only.renderer = altered_renderer(&renderer_only.renderer);
            let renderer_key = admit_treatments(&mut engine, renderer_only.clone());
            treatment_success(&mut engine, renderer_key);
            engine.poll();
            assert_eq!(renderer_key.attempt, old);

            // First SET rejection B is untouched: the prior stays active and B
            // remains the primary failure.
            let mut candidate = renderer_only.clone();
            candidate.renderer = altered_renderer(&candidate.renderer);
            let key = admit_treatments(&mut engine, candidate);
            engine
                .runner
                .events
                .push_back(SessionEvent::TreatmentResult {
                    key,
                    result: Err(renderer_error(
                        key,
                        RendererProperty::Dither,
                        RendererErrorKind::Rejected { mpv_error: -5 },
                        false,
                    )),
                });
            engine.poll();
            assert_eq!(
                engine.model().phase(),
                ProductPhase::ErrorWithActiveRestored
            );
            assert_eq!(
                engine.model().active().unwrap().applied().settings(),
                &renderer_only
            );
            assert_eq!(
                renderer_property_of(
                    engine
                        .model()
                        .failures()
                        .unwrap()
                        .candidate
                        .as_ref()
                        .unwrap()
                ),
                Some(RendererProperty::Dither)
            );

            // A later native proof fault cannot be dropped because a newer
            // unsuccessful renderer attempt is on record.
            engine
                .runner
                .events
                .push_back(SessionEvent::TreatmentFault {
                    key: renderer_key,
                    failure: applied_treatment_fault(renderer_key),
                });
            engine.poll();
            assert_eq!(
                engine.model().treating().unwrap().route(),
                Some(TreatmentRestoreRoute::FreshOwner)
            );
            assert_eq!(engine.runner.stops.len(), 1);
            let failures = engine.model().failures().unwrap();
            assert_eq!(
                renderer_property_of(failures.candidate.as_ref().unwrap()),
                Some(RendererProperty::Dither),
                "B stays the primary failure report"
            );
            let incumbent = failures.incumbent.as_ref().unwrap();
            assert_eq!(
                incumbent.treatment.as_deref().unwrap().key,
                Some(renderer_key)
            );
            assert_eq!(treatment_of(incumbent).unwrap().key, Some(renderer_key));

            engine.runner.barrier(old);
            engine.poll();
            assert_eq!(
                engine.model().validation_request().unwrap().key.purpose,
                AttemptPurpose::Restore
            );
            assert_eq!(
                engine.model().validation_request().unwrap().settings,
                renderer_only
            );
            validate(&mut engine);
            let restore = engine.model().opening().unwrap().0;
            if restore_ok {
                engine.runner.verified();
                engine.poll();
                assert_eq!(
                    engine.model().phase(),
                    ProductPhase::ErrorWithActiveRestored
                );
                assert_eq!(
                    engine.model().last_valid().unwrap().settings(),
                    &renderer_only
                );
                assert_eq!(
                    engine
                        .model()
                        .confirmed_treatments()
                        .unwrap()
                        .renderer()
                        .settings(),
                    &renderer_only.renderer
                );
                let report = engine.model().failures().unwrap();
                assert_eq!(
                    renderer_property_of(report.candidate.as_ref().unwrap()),
                    Some(RendererProperty::Dither)
                );
                assert!(report.incumbent.is_some());
            } else {
                engine
                    .runner
                    .fail("restore of the renderer-only prior failed");
                engine.poll();
                engine.runner.barrier(restore.attempt);
                engine.poll();
                assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
                let report = engine.model().failures().unwrap();
                assert_eq!(
                    report
                        .candidate
                        .as_ref()
                        .unwrap()
                        .treatment
                        .as_deref()
                        .unwrap()
                        .key,
                    Some(key),
                    "the primary untouched failure survives an unavailable restore"
                );
                assert!(report.restore.is_some());
                assert!(report.incumbent.is_some());
            }
            assert_eq!(engine.runner.opens.len(), 2);
        }
    }

    #[test]
    fn retained_renderer_positive_is_revoked_by_later_health_loss_and_lifecycle_cancellation() {
        for cancellation in 0..5 {
            let mut engine = engine();
            initial(&mut engine);
            engine.take_verified_applied();
            let key = admit_treatments(&mut engine, renderer_settings(18));
            treatment_success(&mut engine, key);
            engine.poll();
            assert!(engine.has_user_apply_result_or_pending(key.apply));
            match cancellation {
                0 => {
                    engine
                        .runner
                        .events
                        .push_back(SessionEvent::TreatmentFault {
                            key,
                            failure: treatment_error(
                                key,
                                TreatmentErrorKind::Unconfirmed {
                                    reason: TreatmentConfirmationFailure::BackendUnavailable,
                                },
                                true,
                            ),
                        });
                    engine.poll();
                }
                1 => engine.close(engine.model().state_identity()).unwrap(),
                2 => engine.quit(),
                3 => {
                    engine.restart(engine.model().state_identity()).unwrap();
                }
                4 => {
                    let absent = observation(
                        &engine,
                        2,
                        crate::domain::capture::VideoPresence::Absent,
                        SourcePresence::Disabled,
                        Some(2),
                    );
                    observe(&mut engine, absent);
                }
                _ => unreachable!(),
            }
            assert!(!engine.has_user_apply_result_or_pending(key.apply));
            assert!(engine.take_verified_applied().is_none());
            treatment_success(&mut engine, key);
            engine.poll();
            assert!(
                engine.take_verified_applied().is_none(),
                "stale success cannot reauthorize save"
            );
        }
    }
    #[test]
    fn identical_renderer_apply_is_transaction_but_explicit_dirty_capture_is_rejected() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let ApplyAdmission::Treatments { key, .. } = engine
            .apply(
                engine.model().state_identity(),
                engine.model().draft().revision,
            )
            .unwrap()
        else {
            panic!("identical renderer replay must use live treatments")
        };
        assert_eq!(engine.runner.treatment_intents.len(), 1);
        assert_eq!(engine.runner.treatment_intents[0].1, settings(60));
        assert_eq!(
            engine.restart(engine.model().state_identity()),
            Err(CommandRejection::ApplyInProgress)
        );
        assert_eq!(
            engine.resume(engine.model().state_identity(), old),
            Err(CommandRejection::ApplyInProgress)
        );
        assert_eq!(
            engine.apply_treatments(
                engine.model().state_identity(),
                engine.model().draft().revision
            ),
            Err(CommandRejection::ApplyInProgress)
        );
        treatment_success(&mut engine, key);
        engine.poll();
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        let identity = engine.model().state_identity();
        assert_eq!(
            engine.apply_treatments(identity, engine.model().draft().revision),
            Err(CommandRejection::CaptureDraftChanged)
        );
        assert_eq!(engine.model().state_identity(), identity);
        assert_eq!(engine.runner.treatment_intents.len(), 1);
        assert!(engine.runner.stops.is_empty());
    }
    #[test]
    fn live_prevalidation_refusal_keeps_draft_and_healthy_incumbent_without_owner_work() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let candidate = renderer_settings(19);
        engine.runner.treatment_prepare_failure = Some(
            ApplyFailure::new(
                FailureCategory::Validation(ValidationLayer::Renderer),
                Stage::Prevalidation,
                Cause::Generic,
                candidate.clone(),
                "renderer_prepare",
                "qualified catalog unavailable",
            )
            .with_treatment(TreatmentFailure {
                key: None,
                kind: renderer_kind(RendererErrorKind::CatalogUnavailable),
                disposition: TreatmentFailureDisposition::Untouched,
            }),
        );
        let key = admit_treatments(&mut engine, candidate.clone());
        let failure = engine
            .model()
            .failures()
            .unwrap()
            .candidate
            .as_ref()
            .unwrap();
        assert_eq!(failure.treatment.as_ref().unwrap().key, Some(key));
        assert_eq!(
            treatment_of(failure).unwrap().kind,
            renderer_kind(RendererErrorKind::CatalogUnavailable)
        );
        assert_eq!(
            failure.category,
            FailureCategory::Validation(ValidationLayer::Renderer)
        );
        assert_eq!(engine.model().active().unwrap().attempt(), old);
        assert!(engine.model().draft().settings.same_request(&candidate));
        assert_eq!(engine.runner.opens.len(), 1);
        assert!(engine.runner.stops.is_empty());
        assert!(engine.runner.treatment_intents.is_empty());
        assert!(!engine.has_user_apply_result_or_pending(key.apply));
    }
    #[test]
    fn clean_native_submission_and_command_rejections_never_restore_healthy_incumbent() {
        for kind in [
            renderer_kind(RendererErrorKind::Submission {
                operation: RendererOperation::Set,
                mpv_error: -4,
            }),
            renderer_kind(RendererErrorKind::Rejected { mpv_error: -5 }),
        ] {
            let mut engine = engine();
            let old = initial(&mut engine);
            let candidate = renderer_settings(20);
            let key = admit_treatments(&mut engine, candidate.clone());
            treatment_failed(&mut engine, key, kind.clone(), false);
            engine.poll();
            assert_eq!(
                engine.model().phase(),
                ProductPhase::ErrorWithActiveRestored
            );
            assert_eq!(engine.model().active().unwrap().attempt(), old);
            assert_eq!(
                engine.model().last_valid().unwrap().settings(),
                &settings(60)
            );
            assert!(engine.model().draft().settings.same_request(&candidate));
            assert_eq!(engine.runner.treatment_intents.len(), 1);
            assert!(engine.runner.stops.is_empty());
            assert_eq!(engine.runner.opens.len(), 1);
            assert_eq!(
                treatment_of(
                    engine
                        .model()
                        .failures()
                        .unwrap()
                        .candidate
                        .as_ref()
                        .unwrap()
                )
                .unwrap()
                .kind,
                kind
            );
        }
    }
    #[test]
    fn accepted_unconfirmed_candidate_restores_prior_live_once_and_preserves_newer_draft() {
        let mut engine = engine();
        initial(&mut engine);
        let candidate = renderer_settings(21);
        let key = admit_treatments(&mut engine, candidate.clone());
        let candidate_identity = engine.model().state_identity();
        treatment_failed(&mut engine, key, deadline(), false);
        engine.poll();
        let restore = engine.model().treating().unwrap().key();
        assert_eq!(restore.pass, TreatmentPass::Restore);
        assert_eq!(
            engine.model().state_identity().treatment_pass(),
            Some(TreatmentPass::Restore)
        );
        assert_eq!(
            engine.close(candidate_identity),
            Err(CommandRejection::StaleState)
        );
        assert_eq!(engine.runner.treatment_intents.len(), 2);
        assert_eq!(engine.runner.treatment_intents[1].1, settings(60));
        assert!(engine.runner.stops.is_empty());
        engine
            .edit_draft(engine.model().draft().revision, renderer_settings(15))
            .unwrap();
        treatment_success(&mut engine, key);
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::RestoringTreatments);
        treatment_success(&mut engine, restore);
        engine.poll();
        assert_eq!(
            engine.model().phase(),
            ProductPhase::ErrorWithActiveRestored
        );
        assert_eq!(
            engine.model().last_valid().unwrap().settings(),
            &settings(60)
        );
        assert!(
            engine
                .model()
                .draft()
                .settings
                .same_request(&renderer_settings(15))
        );
        assert!(engine.take_verified_applied().is_none());
        assert!(!engine.has_user_apply_result_or_pending(key.apply));
        assert_eq!(engine.runner.opens.len(), 1);
    }
    #[test]
    fn every_live_restore_failure_drains_without_second_route_or_fresh_restore_open() {
        for failure_mode in 0..7 {
            let mut engine = engine();
            let old = initial(&mut engine);
            let key = admit_treatments(&mut engine, renderer_settings(21));
            if failure_mode == 0 {
                engine.runner.immediate = Some(SubmitStatus::CapacityExceeded);
            }
            treatment_failed(&mut engine, key, deadline(), false);
            engine.poll();
            if failure_mode != 0 {
                let restore = engine.model().treating().unwrap().key();
                match failure_mode {
                    1 => treatment_failed(
                        &mut engine,
                        restore,
                        renderer_kind(RendererErrorKind::Rejected { mpv_error: -5 }),
                        false,
                    ),
                    2 => treatment_failed(
                        &mut engine,
                        restore,
                        TreatmentErrorKind::Unconfirmed {
                            reason: TreatmentConfirmationFailure::BackendUnavailable,
                        },
                        true,
                    ),
                    3 => treatment_failed(&mut engine, restore, deadline(), false),
                    4 => engine.runner.events.push_back(SessionEvent::SessionFailed {
                        attempt: old,
                        failure: failure(
                            settings(60),
                            FailureCategory::Session,
                            "terminal restore",
                        ),
                    }),
                    5 => engine.runner.events.push_back(SessionEvent::StreamEnded {
                        attempt: old,
                        reason: 4,
                        error: -1,
                    }),
                    6 => engine.runner.events.push_back(SessionEvent::OwnerStopped {
                        attempt: old,
                        outcome: Ok(()),
                    }),
                    _ => unreachable!(),
                }
                treatment_success(&mut engine, restore);
                engine.poll();
            }
            assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
            assert!(engine.model().active().is_none());
            assert!(engine.model().failures().unwrap().candidate.is_some());
            assert!(engine.model().failures().unwrap().restore.is_some());
            assert!(!engine.model().can_reconnect());
            engine.runner.barrier(old);
            engine.poll();
            assert!(engine.model().validation_request().is_none());
            assert_eq!(engine.runner.opens.len(), 1);
            assert_eq!(engine.runner.treatment_intents.len(), 2);
            assert!(engine.model().can_reconnect());
        }
    }
    #[test]
    fn rejected_candidate_plus_incumbent_fault_selects_one_fresh_restore_in_either_order() {
        for fault_first in [false, true] {
            let mut engine = engine();
            let old = initial(&mut engine);
            let incumbent = engine.model().confirmed_treatment_key().unwrap();
            let key = admit_treatments(&mut engine, renderer_settings(20));
            let fault = SessionEvent::TreatmentFault {
                key: incumbent,
                failure: treatment_error(
                    incumbent,
                    TreatmentErrorKind::Unconfirmed {
                        reason: TreatmentConfirmationFailure::BackendUnavailable,
                    },
                    true,
                ),
            };
            if fault_first {
                engine.runner.events.push_back(fault);
            }
            treatment_failed(
                &mut engine,
                key,
                renderer_kind(RendererErrorKind::Rejected { mpv_error: -5 }),
                false,
            );
            if !fault_first {
                engine
                    .runner
                    .events
                    .push_back(SessionEvent::TreatmentFault {
                        key: incumbent,
                        failure: treatment_error(
                            incumbent,
                            TreatmentErrorKind::Unconfirmed {
                                reason: TreatmentConfirmationFailure::BackendUnavailable,
                            },
                            true,
                        ),
                    });
            }
            engine.poll();
            assert_eq!(
                engine.model().treating().unwrap().route(),
                Some(TreatmentRestoreRoute::FreshOwner)
            );
            assert_eq!(engine.runner.stops.len(), 1);
            assert_eq!(engine.runner.treatment_intents.len(), 1);
            let failures = engine.model().failures().unwrap();
            assert_eq!(
                treatment_of(failures.candidate.as_ref().unwrap())
                    .unwrap()
                    .kind,
                renderer_kind(RendererErrorKind::Rejected { mpv_error: -5 })
            );
            assert!(failures.incumbent.is_some());
            engine.runner.barrier(old);
            engine.poll();
            assert_eq!(
                engine.model().validation_request().unwrap().key.purpose,
                AttemptPurpose::Restore
            );
            validate(&mut engine);
            engine.runner.verified();
            engine.poll();
            assert_eq!(
                engine.model().phase(),
                ProductPhase::ErrorWithActiveRestored
            );
            assert_eq!(engine.runner.opens.len(), 2);
        }
    }
    #[test]
    fn selected_live_restore_queue_refusal_never_transfers_to_fresh_owner() {
        for status in [
            SubmitStatus::NotReady,
            SubmitStatus::Closing,
            SubmitStatus::StaleGeneration,
            SubmitStatus::CapacityExceeded,
        ] {
            let mut engine = engine();
            let old = initial(&mut engine);
            let key = admit_treatments(&mut engine, renderer_settings(21));
            engine.runner.immediate = Some(status);
            treatment_failed(&mut engine, key, deadline(), false);
            engine.poll();
            assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
            assert!(engine.model().failures().unwrap().restore.is_some());
            engine.runner.barrier(old);
            engine.poll();
            assert_eq!(engine.runner.opens.len(), 1);
            assert_eq!(engine.runner.treatment_intents.len(), 2);
            assert!(engine.model().validation_request().is_none());
        }
    }
    #[test]
    fn source_apply_authorization_survives_closing_old_but_not_failed_candidate_cleanup() {
        let mut engine = engine();
        let old = initial(&mut engine);
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        let admission = engine
            .apply(
                engine.model().state_identity(),
                engine.model().draft().revision,
            )
            .unwrap();
        assert!(matches!(admission, ApplyAdmission::Open { .. }));
        assert!(engine.has_user_apply_result_or_pending(admission.id()));
        validate(&mut engine);
        assert_eq!(engine.model().phase(), ProductPhase::ClosingOld);
        assert!(engine.has_user_apply_result_or_pending(admission.id()));
        engine.runner.barrier(old);
        engine.poll();
        assert!(engine.has_user_apply_result_or_pending(admission.id()));
        engine.runner.fail("candidate failed");
        engine.poll();
        assert!(!engine.has_user_apply_result_or_pending(admission.id()));
    }
    #[test]
    fn deferred_cleanup_preserves_genuine_incumbent_cause_and_actual_retirement_without_second_route()
     {
        for cause_mode in 0..3 {
            for typed_incumbent in [false, true] {
                for acknowledged in [false, true] {
                    for release_in_drain in [false, true] {
                        for retirement_error in [false, true] {
                            let mut engine = engine();
                            let prior = renderer_settings(22);
                            engine
                                .edit_draft(engine.model().draft().revision, prior.clone())
                                .unwrap();
                            let old = initial(&mut engine);
                            engine.take_verified_applied();
                            let confirmed = engine.model().confirmed_treatment_key().unwrap();
                            let expected_candidate = if cause_mode == 2 {
                                let native = treatment_error(
                                    confirmed,
                                    TreatmentErrorKind::Unconfirmed {
                                        reason: TreatmentConfirmationFailure::BackendUnavailable,
                                    },
                                    true,
                                );
                                let expected =
                                    Engine::treatment_failure(prior.clone(), *native.clone());
                                engine
                                    .runner
                                    .events
                                    .push_back(SessionEvent::TreatmentFault {
                                        key: confirmed,
                                        failure: native,
                                    });
                                expected
                            } else {
                                let candidate = renderer_settings(23);
                                let key = admit_treatments(&mut engine, candidate.clone());
                                let kind = if cause_mode == 0 {
                                    renderer_kind(RendererErrorKind::Rejected { mpv_error: -5 })
                                } else {
                                    TreatmentErrorKind::Unconfirmed {
                                        reason: TreatmentConfirmationFailure::BackendUnavailable,
                                    }
                                };
                                // Promote the aggregate disposition only when
                                // independent terminal/incumbent evidence proves
                                // this owner can no longer preserve the prior.
                                let mut expected_native =
                                    treatment_error(key, kind.clone(), cause_mode != 0);
                                expected_native.disposition =
                                    TreatmentFailureDisposition::RestoreFreshOwner;
                                let expected =
                                    Engine::treatment_failure(candidate, *expected_native);
                                treatment_failed(&mut engine, key, kind, cause_mode != 0);
                                expected
                            };
                            let incumbent = if typed_incumbent {
                                Engine::treatment_failure(
                                    prior.clone(),
                                    *treatment_error(
                                        confirmed,
                                        TreatmentErrorKind::Unconfirmed {
                                            reason: TreatmentConfirmationFailure::EvidenceLost,
                                        },
                                        true,
                                    ),
                                )
                            } else {
                                ApplyFailure::new(
                                    FailureCategory::Session,
                                    Stage::Negotiation,
                                    Cause::Busy,
                                    prior.clone(),
                                    "genuine_incumbent_failure",
                                    "independent native incumbent error",
                                )
                            };
                            let outcome_error = failure(
                                prior.clone(),
                                FailureCategory::Lifecycle(LifecycleFailure::Quiescence),
                                "distinct actual retirement outcome",
                            );
                            engine.runner.events.push_back(SessionEvent::SessionFailed {
                                attempt: old,
                                failure: incumbent.clone(),
                            });
                            if acknowledged {
                                engine.runner.events.push_back(SessionEvent::OwnerStopped {
                                    attempt: old,
                                    outcome: if retirement_error {
                                        Err(outcome_error.clone())
                                    } else {
                                        Ok(())
                                    },
                                });
                            }
                            if release_in_drain {
                                engine
                                    .runner
                                    .events
                                    .push_back(SessionEvent::NativeReleased { attempt: old });
                            }
                            let newer = renderer_settings(24);
                            engine
                                .edit_draft(engine.model().draft().revision, newer.clone())
                                .unwrap();
                            engine.poll();
                            let report = engine.model().failures().unwrap();
                            assert_eq!(report.candidate.as_ref(), Some(&expected_candidate));
                            assert_eq!(
                                report.incumbent.as_ref(),
                                Some(&incumbent),
                                "logical cleanup cannot manufacture a different incumbent error"
                            );
                            if let Some(treatment) = treatment_of(&incumbent) {
                                assert_eq!(
                                    treatment_of(report.incumbent.as_ref().unwrap())
                                        .unwrap()
                                        .key,
                                    treatment.key
                                );
                            }
                            assert_eq!(
                                report.cleanup,
                                if acknowledged && retirement_error {
                                    vec![outcome_error.clone()]
                                } else {
                                    Vec::new()
                                }
                            );
                            assert_eq!(engine.model().last_valid().unwrap().settings(), &prior);
                            assert!(engine.model().draft().settings.same_request(&newer));
                            assert_eq!(engine.runner.opens.len(), 1);
                            if acknowledged && release_in_drain {
                                assert_eq!(
                                    engine.model().validation_request().unwrap().key.purpose,
                                    AttemptPurpose::Restore
                                );
                            } else {
                                assert!(
                                    engine.model().validation_request().is_none(),
                                    "logical stop cannot impersonate actual retirement/native release"
                                );
                            }
                            if acknowledged {
                                assert!(
                                    engine.runner.stops.is_empty(),
                                    "a genuinely retired owner must not receive a redundant Stop"
                                );
                            } else {
                                assert_eq!(
                                    engine.runner.stops,
                                    vec![(old, StopReason::Failed)],
                                    "logical selection still requires physical Stop without acknowledgement"
                                );
                                engine.runner.events.push_back(SessionEvent::OwnerStopped {
                                    attempt: old,
                                    outcome: if retirement_error {
                                        Err(outcome_error.clone())
                                    } else {
                                        Ok(())
                                    },
                                });
                            }
                            if !acknowledged || !release_in_drain {
                                engine
                                    .runner
                                    .events
                                    .push_back(SessionEvent::NativeReleased { attempt: old });
                                engine.poll();
                            }
                            assert_eq!(
                                engine.model().failures().unwrap().incumbent.as_ref(),
                                Some(&incumbent)
                            );
                            assert_eq!(
                                engine.model().validation_request().unwrap().settings,
                                prior
                            );
                            validate(&mut engine);
                            let restore = engine.model().opening().unwrap().0;
                            assert_eq!(restore.purpose, AttemptPurpose::Restore);
                            engine.runner.fail("one chosen fresh Restore failed");
                            engine.poll();
                            engine.runner.barrier(restore.attempt);
                            engine.poll();
                            assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
                            let report = engine.model().failures().unwrap();
                            assert_eq!(report.candidate.as_ref(), Some(&expected_candidate));
                            assert_eq!(report.incumbent.as_ref(), Some(&incumbent));
                            assert!(report.restore.is_some());
                            assert_eq!(
                                report.cleanup,
                                if retirement_error {
                                    vec![outcome_error]
                                } else {
                                    Vec::new()
                                }
                            );
                            assert!(engine.model().validation_request().is_none());
                            assert_eq!(engine.validator.requests.iter().filter(|request| request.key.purpose == AttemptPurpose::Restore).count(), 1);
                            assert_eq!(
                                engine.runner.opens.len(),
                                2,
                                "no alternate restoration route or retry"
                            );
                            assert!(engine.model().draft().settings.same_request(&newer));
                            assert!(engine.take_verified_applied().is_none());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn candidate_rejection_survives_terminal_and_release_in_same_drain() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let key = admit_treatments(&mut engine, renderer_settings(20));
        treatment_failed(
            &mut engine,
            key,
            renderer_kind(RendererErrorKind::Rejected { mpv_error: -5 }),
            false,
        );
        engine.runner.barrier(old);
        engine.poll();
        let request = engine.model().validation_request().unwrap();
        assert_eq!(request.key.purpose, AttemptPurpose::Restore);
        assert_eq!(
            treatment_of(
                engine
                    .model()
                    .failures()
                    .unwrap()
                    .candidate
                    .as_ref()
                    .unwrap()
            )
            .unwrap()
            .kind,
            renderer_kind(RendererErrorKind::Rejected { mpv_error: -5 })
        );
        assert_eq!(engine.runner.opens.len(), 1);
    }
    #[test]
    fn native_proof_loss_awaits_both_barriers_and_failed_fresh_restore_never_retries() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let key = admit_treatments(&mut engine, renderer_settings(25));
        treatment_failed(
            &mut engine,
            key,
            TreatmentErrorKind::Unconfirmed {
                reason: TreatmentConfirmationFailure::BackendUnavailable,
            },
            true,
        );
        treatment_success(&mut engine, key);
        engine.poll();
        assert_eq!(engine.runner.stops.len(), 1);
        assert_eq!(engine.runner.treatment_intents.len(), 1);
        assert!(engine.model().validation_request().is_none());
        engine
            .runner
            .events
            .push_back(SessionEvent::NativeReleased { attempt: old });
        engine.poll();
        assert!(engine.model().validation_request().is_none());
        engine.runner.barrier(old);
        engine.poll();
        assert_eq!(
            engine.model().validation_request().unwrap().settings,
            settings(60)
        );
        validate(&mut engine);
        let restore = engine.model().opening().unwrap().0;
        assert_eq!(restore.purpose, AttemptPurpose::Restore);
        engine.runner.fail("fresh restore native failure");
        engine.poll();
        engine.runner.barrier(restore.attempt);
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
        assert!(engine.model().failures().unwrap().restore.is_some());
        assert!(engine.model().validation_request().is_none());
        assert_eq!(engine.runner.opens.len(), 2);
        assert!(engine.model().can_reconnect());
    }
    #[test]
    fn late_fault_restores_then_last_verified_renderer_not_older_history_or_new_draft() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let applied = renderer_settings(26);
        let key = admit_treatments(&mut engine, applied.clone());
        treatment_success(&mut engine, key);
        engine.poll();
        engine
            .edit_draft(engine.model().draft().revision, renderer_settings(15))
            .unwrap();
        engine
            .runner
            .events
            .push_back(SessionEvent::TreatmentFault {
                key,
                failure: treatment_error(
                    key,
                    TreatmentErrorKind::Unconfirmed {
                        reason: TreatmentConfirmationFailure::EvidenceLost,
                    },
                    true,
                ),
            });
        engine.poll();
        assert_eq!(
            engine.model().treating().unwrap().prior().settings(),
            &applied
        );
        assert_eq!(engine.runner.stops.len(), 1);
        engine.runner.barrier(old);
        engine.poll();
        assert_eq!(
            engine.model().validation_request().unwrap().settings,
            applied
        );
        validate(&mut engine);
        engine.runner.verified();
        engine.poll();
        assert_eq!(
            engine.model().phase(),
            ProductPhase::ErrorWithActiveRestored
        );
        assert_eq!(engine.model().last_valid().unwrap().settings(), &applied);
        assert!(
            engine
                .model()
                .draft()
                .settings
                .same_request(&renderer_settings(15))
        );
        assert_eq!(engine.runner.opens.len(), 2);
        assert_eq!(engine.runner.treatment_intents.len(), 1);
    }
    #[test]
    fn late_fault_and_terminal_same_drain_fail_one_fresh_restore_without_older_fallback() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let applied = renderer_settings(27);
        let key = admit_treatments(&mut engine, applied.clone());
        treatment_success(&mut engine, key);
        engine.poll();
        engine.runner.events.push_back(SessionEvent::SessionFailed {
            attempt: old,
            failure: failure(applied.clone(), FailureCategory::Session, "terminal owner"),
        });
        engine
            .runner
            .events
            .push_back(SessionEvent::TreatmentFault {
                key,
                failure: treatment_error(
                    key,
                    TreatmentErrorKind::Unconfirmed {
                        reason: TreatmentConfirmationFailure::BackendUnavailable,
                    },
                    true,
                ),
            });
        engine.poll();
        assert_eq!(
            engine.model().treating().unwrap().prior().settings(),
            &applied
        );
        assert_eq!(engine.runner.stops.len(), 1);
        engine.runner.barrier(old);
        engine.poll();
        assert_eq!(
            engine.model().validation_request().unwrap().settings,
            applied
        );
        validate(&mut engine);
        let restore = engine.model().opening().unwrap().0;
        engine
            .runner
            .fail("then verified renderer also fails on fresh owner");
        engine.poll();
        engine.runner.barrier(restore.attempt);
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
        assert!(engine.model().failures().unwrap().candidate.is_some());
        assert!(engine.model().failures().unwrap().restore.is_some());
        assert_eq!(engine.model().last_valid().unwrap().settings(), &applied);
        assert_eq!(engine.runner.opens.len(), 2);
        assert!(engine.model().validation_request().is_none());
    }
    #[test]
    fn close_quit_and_removal_cancel_both_selected_restoration_routes() {
        for fresh in [false, true] {
            for cancellation in 0..3 {
                let mut engine = engine();
                let prior = renderer_settings(28);
                engine
                    .edit_draft(engine.model().draft().revision, prior.clone())
                    .unwrap();
                let old = initial(&mut engine);
                let key = admit_treatments(&mut engine, renderer_settings(6));
                treatment_failed(
                    &mut engine,
                    key,
                    if fresh {
                        TreatmentErrorKind::Unconfirmed {
                            reason: TreatmentConfirmationFailure::BackendUnavailable,
                        }
                    } else {
                        deadline()
                    },
                    fresh,
                );
                engine.poll();
                let restore = engine.model().treating().unwrap().key();
                let submitted = engine.runner.treatment_intents.len();
                treatment_success(&mut engine, restore);
                match cancellation {
                    0 => engine.close(engine.model().state_identity()).unwrap(),
                    1 => engine.quit(),
                    2 => {
                        engine.validator.observation = Some(observation(
                            &engine,
                            2,
                            crate::domain::capture::VideoPresence::Present,
                            SourcePresence::Disabled,
                            Some(2),
                        ))
                    }
                    _ => unreachable!(),
                }
                engine.poll();
                assert_eq!(engine.model().last_valid().unwrap().settings(), &prior);
                assert!(engine.model().treating().is_none());
                assert_eq!(engine.runner.treatment_intents.len(), submitted);
                engine.runner.barrier(old);
                engine.poll();
                assert!(
                    engine
                        .model()
                        .validation_request()
                        .is_none_or(|request| request.key.purpose != AttemptPurpose::Restore)
                );
                assert_eq!(engine.runner.opens.len(), 1);
                assert!(!engine.has_user_apply_result_or_pending(key.apply));
                assert!(engine.take_verified_applied().is_none());
                if cancellation == 2 {
                    assert_eq!(
                        engine.model().recovery().unwrap().applied.settings(),
                        &prior
                    );
                }
            }
        }
    }

    #[test]
    fn close_quit_and_physical_loss_cancel_pending_renderer_commit_and_restore() {
        for cancellation in 0..4 {
            let mut engine = engine();
            let prior = renderer_settings(28);
            engine
                .edit_draft(engine.model().draft().revision, prior.clone())
                .unwrap();
            let old = initial(&mut engine);
            let key = admit_treatments(&mut engine, renderer_settings(29));
            treatment_success(&mut engine, key);
            match cancellation {
                0 => engine.close(engine.model().state_identity()).unwrap(),
                1 => engine.quit(),
                2 => engine.runner.events.push_back(SessionEvent::StreamEnded {
                    attempt: old,
                    reason: 0,
                    error: 0,
                }),
                3 => {
                    engine.validator.observation = Some(observation(
                        &engine,
                        2,
                        crate::domain::capture::VideoPresence::Present,
                        SourcePresence::Disabled,
                        Some(2),
                    ));
                }
                _ => unreachable!(),
            }
            engine.poll();
            assert_eq!(engine.model().last_valid().unwrap().settings(), &prior);
            assert!(!engine.has_user_apply_result_or_pending(key.apply));
            assert!(engine.take_verified_applied().is_none());
            assert_eq!(engine.runner.treatment_intents.len(), 1);
            assert_eq!(engine.runner.opens.len(), 1);
            if cancellation >= 2 {
                assert_eq!(
                    engine.model().recovery().unwrap().applied.settings(),
                    &prior
                );
            }
        }
    }
    #[test]
    fn wrong_operation_owner_pass_or_confirmation_key_cannot_commit() {
        for mismatch in 0..4 {
            let mut engine = engine();
            initial(&mut engine);
            let key = admit_treatments(&mut engine, renderer_settings(30));
            let mut wrong = key;
            match mismatch {
                0 => wrong.apply = ApplyId::new(key.apply.get() + 1).unwrap(),
                1 => wrong.attempt = AttemptId::new(key.attempt.get() + 1).unwrap(),
                2 => wrong.pass = TreatmentPass::Restore,
                3 => {}
                _ => unreachable!(),
            }
            let proof_key = if mismatch == 3 {
                TreatmentAttemptKey {
                    pass: TreatmentPass::Open,
                    ..key
                }
            } else {
                wrong
            };
            let proof = TreatmentConfirmation::checked(
                proof_key,
                InitialPlayback::Live,
                RendererConfirmation::checked(proof_key, renderer_settings(30).renderer),
            )
            .unwrap();
            engine
                .runner
                .events
                .push_back(SessionEvent::TreatmentResult {
                    key: wrong,
                    result: Ok(proof),
                });
            engine.poll();
            assert_eq!(engine.model().phase(), ProductPhase::ApplyingTreatments);
            assert_eq!(
                engine.model().last_valid().unwrap().settings(),
                &settings(60)
            );
        }
    }

    #[test]
    fn source_renderer_prevalidation_refusal_preserves_incumbent_before_stop() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let prior = engine.model().last_valid().unwrap().clone();
        let mut candidate = settings(30);
        candidate.renderer = renderer_settings(40).renderer;
        engine
            .edit_draft(engine.model().draft().revision, candidate.clone())
            .unwrap();
        let failure = ApplyFailure::new(
            FailureCategory::Validation(ValidationLayer::Renderer),
            Stage::Prevalidation,
            Cause::Generic,
            candidate.clone(),
            "renderer_prepare",
            "catalog unavailable",
        )
        .with_treatment(TreatmentFailure {
            key: None,
            kind: renderer_kind(RendererErrorKind::CatalogUnavailable),
            disposition: TreatmentFailureDisposition::Untouched,
        });
        engine.runner.treatment_prepare_failure = Some(failure.clone());
        apply(&mut engine);
        validate(&mut engine);
        assert_eq!(engine.model().active().unwrap().attempt(), old);
        assert_eq!(engine.model().last_valid(), Some(&prior));
        assert!(engine.model().draft().settings.same_request(&candidate));
        assert!(engine.runner.stops.is_empty());
        assert_eq!(engine.runner.opens.len(), 1);
        assert_eq!(
            engine.runner.treatment_preparations.last(),
            Some(&candidate)
        );
        assert_eq!(
            engine.model().validation_rejection().unwrap().failure,
            failure
        );
    }
    #[test]
    fn source_open_refuses_wrong_renderer_operation_owner_pass_or_paused_preparation() {
        for mismatch in 0..6 {
            let mut engine = engine();
            let requested = renderer_settings(31);
            engine
                .edit_draft(engine.model().draft().revision, requested.clone())
                .unwrap();
            apply(&mut engine);
            validate(&mut engine);
            let key = engine.model().opening().unwrap().0;
            let mut received = receipt(requested.clone(), key);
            let open_key = TreatmentAttemptKey {
                apply: key.apply,
                attempt: key.attempt,
                pass: TreatmentPass::Open,
            };
            match mismatch {
                3 => {
                    let preparation = paused_treatments(key, &received.settings);
                    received.treatments = preparation;
                }
                5 => {
                    let preparation = paused_treatments(key, &received.settings);
                    received.treatments = preparation;
                    received.readiness = OpenReadiness::PausedPrepared;
                }
                4 => {
                    received.settings.renderer = RendererSettings::default();
                }
                _ => {
                    let mut observed = open_key;
                    match mismatch {
                        0 => observed.apply = ApplyId::new(key.apply.get() + 1).unwrap(),
                        1 => observed.attempt = AttemptId::new(key.attempt.get() + 1).unwrap(),
                        2 => observed.pass = TreatmentPass::Candidate,
                        _ => unreachable!(),
                    }
                    let renderer =
                        RendererConfirmation::checked(observed, received.settings.renderer.clone());
                    let proof =
                        TreatmentConfirmation::checked(observed, InitialPlayback::Live, renderer)
                            .unwrap();
                    received.treatments = proof;
                }
            }
            engine.runner.events.push_back(SessionEvent::OpenVerified {
                key,
                receipt: Box::new(received),
            });
            engine.poll();
            assert!(engine.model().active().is_none());
            assert!(engine.model().last_valid().is_none());
            assert!(engine.take_verified_applied().is_none());
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
            assert_eq!(engine.runner.stops.len(), 1);
        }
    }
    #[test]
    fn paused_recovery_receipt_cannot_drop_or_change_frozen_treatments() {
        let mut engine = engine();
        let prior = renderer_settings(32);
        engine
            .edit_draft(engine.model().draft().revision, prior.clone())
            .unwrap();
        let old = initial(&mut engine);
        engine.take_verified_applied();
        confirm_pause(&mut engine, old);
        engine
            .edit_draft(engine.model().draft().revision, renderer_settings(33))
            .unwrap();
        eof(&mut engine, old);
        engine.runner.barrier(old);
        engine.poll();
        validate(&mut engine);
        let key = engine.runner.opens.last().unwrap().0;
        assert_eq!(
            *engine.runner.playback.last().unwrap(),
            InitialPlayback::Paused
        );
        let mut received = receipt(prior.clone(), key);
        received.readiness = OpenReadiness::PausedPrepared;
        received.treatments = paused_treatments(key, &prior);
        received.settings.renderer = RendererSettings::default();
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(received),
        });
        engine.poll();
        assert!(engine.model().active().is_none());
        assert_eq!(engine.model().last_valid().unwrap().settings(), &prior);
        assert!(engine.take_verified_applied().is_none());
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
    fn exact_paused_restore_prepares_prior_without_save_then_resume_requires_fresh_live_proof() {
        for wrong_renderer in [false, true] {
            let mut engine = engine();
            let prior = renderer_settings(34);
            engine
                .edit_draft(engine.model().draft().revision, prior.clone())
                .unwrap();
            let old = initial(&mut engine);
            engine.take_verified_applied();
            confirm_pause(&mut engine, old);
            let mut candidate = renderer_settings(35);
            candidate.video.mode = settings(30).video.mode;
            engine
                .edit_draft(engine.model().draft().revision, candidate.clone())
                .unwrap();
            let admission = engine
                .apply(
                    engine.model().state_identity(),
                    engine.model().draft().revision,
                )
                .unwrap();
            validate(&mut engine);
            engine.runner.barrier(old);
            engine.poll();
            let failed_owner = engine.runner.opens.last().unwrap().0.attempt;
            engine.runner.fail("actual candidate open failure");
            engine.poll();
            engine.runner.barrier(failed_owner);
            engine.poll();
            validate(&mut engine);
            let restore = engine.runner.opens.last().unwrap().0;
            assert_eq!(restore.purpose, AttemptPurpose::Restore);
            assert_eq!(
                *engine.runner.playback.last().unwrap(),
                InitialPlayback::Paused
            );
            assert_eq!(engine.runner.opens.last().unwrap().1, prior);
            let mut received = receipt(prior.clone(), restore);
            received.readiness = OpenReadiness::PausedPrepared;
            received.treatments = paused_treatments(restore, &prior);
            if wrong_renderer {
                received.settings.renderer = RendererSettings::default();
            }
            engine.runner.events.push_back(SessionEvent::OpenVerified {
                key: restore,
                receipt: Box::new(received),
            });
            engine.poll();
            assert!(engine.take_verified_applied().is_none());
            assert!(!engine.has_user_apply_result_or_pending(admission.id()));
            assert_eq!(engine.model().last_valid().unwrap().settings(), &prior);
            assert!(engine.model().draft().settings.same_request(&candidate));
            if wrong_renderer {
                assert!(engine.model().active().is_none());
                assert_eq!(
                    engine
                        .model()
                        .failures()
                        .unwrap()
                        .restore
                        .as_ref()
                        .unwrap()
                        .operation,
                    "open_receipt"
                );
                continue;
            }
            assert_eq!(
                engine.model().phase(),
                ProductPhase::ErrorWithActiveRestored
            );
            assert_eq!(
                engine.model().active().unwrap().playback(),
                PlaybackState::Paused
            );
            engine
                .resume(engine.model().state_identity(), restore.attempt)
                .unwrap();
            validate(&mut engine);
            engine.runner.barrier(restore.attempt);
            engine.poll();
            let resumed = engine.runner.opens.last().unwrap().0;
            assert_eq!(resumed.purpose, AttemptPurpose::Resume);
            assert_eq!(
                *engine.runner.playback.last().unwrap(),
                InitialPlayback::Live
            );
            assert!(engine.model().active().is_none());
            let proof = receipt(prior.clone(), resumed);
            let confirmation = proof.treatments.renderer();
            assert_eq!(confirmation.settings(), &prior.renderer);
            assert_eq!(proof.treatments.playback(), InitialPlayback::Live);
            assert_eq!(
                confirmation.key(),
                TreatmentAttemptKey {
                    apply: resumed.apply,
                    attempt: resumed.attempt,
                    pass: TreatmentPass::Open
                }
            );
            engine.runner.events.push_back(SessionEvent::OpenVerified {
                key: resumed,
                receipt: Box::new(proof),
            });
            engine.poll();
            assert_eq!(
                engine.model().active().unwrap().playback(),
                PlaybackState::Live
            );
            assert_eq!(
                engine.model().active().unwrap().applied().settings(),
                &prior
            );
            assert!(engine.take_verified_applied().is_none());
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
        // Existing source-lifecycle fixtures deliberately exercise a fresh open.
        // Same-source user Apply now has its own renderer transaction tests above.
        if engine.model().active().is_some_and(|active| {
            active.playback() == PlaybackState::Live
                && active.applied().settings().video == engine.model().draft().settings.video
                && active.applied().settings().audio == engine.model().draft().settings.audio
        }) {
            return engine.restart(engine.model().state_identity()).unwrap();
        }
        engine
            .apply(
                engine.model().state_identity(),
                engine.model().draft().revision,
            )
            .unwrap()
            .id()
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

    fn native_node_engine() -> Engine {
        let mut draft = settings(60);
        let mode = draft
            .video
            .mode
            .with_fourcc(CapturedFourCc::from_bytes(*b"YUYV"));
        draft.video.mode.format_policy = crate::domain::capture::InputFormatPolicy::Auto;
        draft.video.resolved_fourcc = None;
        draft.video.selected_node = Some("/dev/video0".into());
        let snapshot =
            crate::capture::linux::session_fixture(&["/dev/video0", "/dev/video1"], mode);
        draft.video.identity = snapshot.devices()[0].identity().clone();
        ApplyCoordinator::new(
            draft,
            PlaybackGain::default(),
            Validator {
                native_snapshot: Some(snapshot),
                ..Validator::default()
            },
            Runner::default(),
        )
    }

    #[test]
    fn selected_sibling_capture_opens_before_reopen_operations_keep_its_physical_pin() {
        let binding = (
            std::path::Path::new("/sys/dev/video1"),
            std::path::Path::new("/sys/usb/fixture"),
        );
        for operation in 0..4 {
            let mut engine = native_node_engine();
            let old = initial(&mut engine);
            let mut candidate = engine.model().draft().settings.clone();
            candidate.video.selected_node = Some("/dev/video1".into());
            let applied = engine.model().active().unwrap().applied().settings();
            assert!(applied.video.same_request(&candidate.video));
            assert_eq!(applied.audio, candidate.audio);
            let revision = engine
                .edit_draft(engine.model().draft().revision, candidate)
                .unwrap();
            let requests = engine.validator.requests.len();
            assert!(matches!(
                engine
                    .apply_selected_capture(engine.model().state_identity(), revision, binding)
                    .unwrap(),
                ApplyAdmission::Open { .. }
            ));
            let request = &engine.validator.requests[requests];
            assert_eq!(
                request
                    .pinned_node
                    .as_ref()
                    .map(|(node, usb)| (node.as_path(), usb.as_path())),
                Some(binding)
            );
            assert_eq!(engine.model().validation_request(), Some(request));
            assert_eq!(engine.model().active().unwrap().attempt(), old);
            assert!(engine.runner.stops.is_empty());
            assert!(engine.runner.treatment_intents.is_empty());
            assert_eq!(engine.runner.opens.len(), 1);
            validate(&mut engine);
            assert_eq!(engine.model().phase(), ProductPhase::ClosingOld);
            assert_eq!(engine.runner.opens.len(), 1);
            engine.runner.barrier(old);
            engine.poll();
            engine.runner.verified();
            engine.poll();
            let active = engine.model().active().unwrap();
            assert_eq!(active.applied().physical_node(), Some(binding));
            let selected = active.attempt();
            match operation {
                0 => {
                    engine.restart(engine.model().state_identity()).unwrap();
                }
                1 => {
                    confirm_pause(&mut engine, selected);
                    engine
                        .resume(engine.model().state_identity(), selected)
                        .unwrap();
                }
                2 | 3 => {
                    eof(&mut engine, selected);
                    engine.runner.barrier(selected);
                    engine.poll();
                    if operation == 3 {
                        let mode = engine
                            .model()
                            .last_valid()
                            .unwrap()
                            .settings()
                            .video
                            .fixture_mode();
                        engine.validator.native_snapshot = Some(
                            crate::capture::linux::session_fixture(&["/dev/video0"], mode),
                        );
                        validate(&mut engine);
                        assert!(engine.model().active().is_none());
                        engine.validator.native_snapshot =
                            Some(crate::capture::linux::session_fixture(
                                &["/dev/video0", "/dev/video1"],
                                mode,
                            ));
                        engine.reconnect(engine.model().state_identity()).unwrap();
                    }
                }
                _ => unreachable!(),
            }
            assert_eq!(
                engine
                    .model()
                    .validation_request()
                    .unwrap()
                    .pinned_node
                    .as_ref()
                    .map(|(node, usb)| (node.as_path(), usb.as_path())),
                Some(binding)
            );
            validate(&mut engine);
            if operation < 2 {
                engine.runner.barrier(selected);
                engine.poll();
            }
            engine.runner.verified();
            engine.poll();
            assert_eq!(
                engine.model().active().unwrap().applied().physical_node(),
                Some(binding),
                "operation {operation}"
            );
        }
    }

    #[test]
    fn selected_missing_sibling_keeps_incumbent_and_failed_open_restores_its_pin() {
        let prior_binding = (
            std::path::Path::new("/sys/dev/video0"),
            std::path::Path::new("/sys/usb/fixture"),
        );
        let selected_binding = (std::path::Path::new("/sys/dev/video1"), prior_binding.1);
        for missing in [true, false] {
            let mut engine = native_node_engine();
            let old = initial(&mut engine);
            let prior = engine.model().last_valid().unwrap().clone();
            let mut candidate = engine.model().draft().settings.clone();
            candidate.video.selected_node = Some("/dev/video1".into());
            let revision = engine
                .edit_draft(engine.model().draft().revision, candidate.clone())
                .unwrap();
            engine
                .apply_selected_capture(engine.model().state_identity(), revision, selected_binding)
                .unwrap();
            if missing {
                engine.validator.native_snapshot = Some(crate::capture::linux::session_fixture(
                    &["/dev/video0"],
                    prior.settings().video.fixture_mode(),
                ));
            }
            validate(&mut engine);
            if missing {
                assert_eq!(engine.model().active().unwrap().attempt(), old);
                assert!(engine.runner.stops.is_empty());
                assert_eq!(engine.runner.opens.len(), 1);
                let rejection = engine.model().validation_rejection().unwrap();
                assert_eq!(rejection.failure.cause, Cause::RequestedModeRefused);
                assert_eq!(
                    rejection
                        .request
                        .pinned_node
                        .as_ref()
                        .map(|(node, usb)| (node.as_path(), usb.as_path())),
                    Some(selected_binding)
                );
            } else {
                engine.runner.barrier(old);
                engine.poll();
                let failed = engine.runner.opens.last().unwrap().0.attempt;
                engine.runner.fail("selected sibling failed");
                engine.poll();
                assert_eq!(engine.model().last_valid(), Some(&prior));
                engine.runner.barrier(failed);
                engine.poll();
                assert_eq!(
                    engine
                        .model()
                        .validation_request()
                        .unwrap()
                        .pinned_node
                        .as_ref()
                        .map(|(node, usb)| (node.as_path(), usb.as_path())),
                    Some(prior_binding)
                );
                validate(&mut engine);
                engine.runner.verified();
                engine.poll();
                assert_eq!(
                    engine.model().phase(),
                    ProductPhase::ErrorWithActiveRestored
                );
                assert_eq!(engine.runner.opens.len(), 3);
            }
            assert_eq!(engine.model().last_valid(), Some(&prior));
            assert_eq!(
                engine.model().active().unwrap().applied().physical_node(),
                Some(prior_binding)
            );
            assert_eq!(engine.model().draft().settings, candidate);
        }
    }

    #[test]
    fn selected_same_source_preserves_live_and_paused_treatment_eligibility_and_pin() {
        let binding = (
            std::path::Path::new("/sys/dev/video0"),
            std::path::Path::new("/sys/usb/fixture"),
        );
        for edit in 0..3 {
            let mut engine = native_node_engine();
            let old = initial(&mut engine);
            if edit == 2 {
                confirm_pause(&mut engine, old);
            }
            let mut candidate = engine.model().draft().settings.clone();
            if edit == 0 {
                candidate.renderer = renderer_settings(36).renderer;
            } else {
                candidate.renderer = altered_renderer(&candidate.renderer);
            }
            let revision = engine
                .edit_draft(engine.model().draft().revision, candidate.clone())
                .unwrap();
            let requests = engine.validator.requests.len();
            let ApplyAdmission::Treatments { key, .. } = engine
                .apply_selected_capture(engine.model().state_identity(), revision, binding)
                .unwrap()
            else {
                panic!("same admitted physical source keeps eligible treatment route");
            };
            assert_eq!(key.attempt, old);
            assert_eq!(engine.validator.requests.len(), requests);
            assert!(engine.model().validation_request().is_none());
            treatment_success(&mut engine, key);
            engine.poll();
            let active = engine.model().active().unwrap();
            assert_eq!(active.attempt(), old);
            assert_eq!(active.applied().physical_node(), Some(binding));
            assert_eq!(active.applied().settings().renderer, candidate.renderer);
            assert_eq!(
                active.playback(),
                if edit == 2 {
                    PlaybackState::Paused
                } else {
                    PlaybackState::Live
                }
            );
            assert_eq!(engine.runner.opens.len(), 1);
            assert!(engine.runner.stops.is_empty());
        }
    }

    #[test]
    fn selected_same_source_full_mode_audio_and_paused_capture_candidates_keep_pin() {
        let binding = (
            std::path::Path::new("/sys/dev/video0"),
            std::path::Path::new("/sys/usb/fixture"),
        );
        for edit in 0..3 {
            let mut engine = native_node_engine();
            let old = initial(&mut engine);
            let mut candidate = engine.model().draft().settings.clone();
            match edit {
                0 => candidate.video.mode.rate = FrameRate::new(30, 1).unwrap(),
                1 => {
                    candidate.audio = AudioSelection::Enabled {
                        source: crate::domain::capture::AudioSourceIdentity::new(
                            "selected.capture".into(),
                            vec![],
                        )
                        .unwrap(),
                    };
                }
                2 => {
                    confirm_pause(&mut engine, old);
                    candidate.video.mode.rate = FrameRate::new(30, 1).unwrap();
                    candidate.renderer = renderer_settings(37).renderer;
                }
                _ => unreachable!(),
            }
            let revision = engine
                .edit_draft(engine.model().draft().revision, candidate.clone())
                .unwrap();
            assert!(matches!(
                engine
                    .apply_selected_capture(engine.model().state_identity(), revision, binding)
                    .unwrap(),
                ApplyAdmission::Open { .. }
            ));
            let request = engine.model().validation_request().unwrap();
            assert_eq!(
                request
                    .pinned_node
                    .as_ref()
                    .map(|(node, usb)| (node.as_path(), usb.as_path())),
                Some(binding)
            );
            assert!(request.settings.video.same_request(&candidate.video));
            assert_eq!(request.settings.audio, candidate.audio);
            assert_eq!(request.settings.renderer, candidate.renderer);
            assert!(request.settings.video.resolved_fourcc.is_none());
            assert_eq!(engine.model().active().unwrap().attempt(), old);
            assert!(engine.runner.stops.is_empty());
            assert!(engine.runner.treatment_intents.is_empty());
        }
    }

    #[test]
    fn selected_source_rejects_stale_state_revision_and_closing_without_effects() {
        let binding = (
            std::path::Path::new("/sys/dev/video1"),
            std::path::Path::new("/sys/usb/fixture"),
        );
        let mut engine = native_node_engine();
        let stale_state = engine.model().state_identity();
        initial(&mut engine);
        let stale_revision = engine.model().draft().revision;
        let revision = engine
            .edit_draft(stale_revision, engine.model().draft().settings.clone())
            .unwrap();
        for stale in [true, false] {
            let state = engine.model().state_identity();
            let requests = engine.validator.requests.len();
            assert_eq!(
                engine.apply_selected_capture(
                    if stale { stale_state } else { state },
                    if stale { revision } else { stale_revision },
                    binding,
                ),
                Err(if stale {
                    CommandRejection::StaleState
                } else {
                    CommandRejection::StaleRevision
                })
            );
            assert_eq!(engine.model().state_identity(), state);
            assert_eq!(engine.validator.requests.len(), requests);
            assert!(engine.model().validation_request().is_none());
            assert_eq!(engine.runner.opens.len(), 1);
            assert!(engine.runner.stops.is_empty());
            assert!(engine.runner.treatment_intents.is_empty());
        }
        engine.close(engine.model().state_identity()).unwrap();
        let state = engine.model().state_identity();
        let requests = engine.validator.requests.len();
        let stops = engine.runner.stops.len();
        assert_eq!(
            engine.apply_selected_capture(state, revision, binding),
            Err(CommandRejection::CleanupIncomplete)
        );
        assert_eq!(engine.model().state_identity(), state);
        assert_eq!(engine.validator.requests.len(), requests);
        assert_eq!(engine.runner.stops.len(), stops);
        assert!(engine.model().validation_request().is_none());
        assert!(engine.runner.treatment_intents.is_empty());
    }

    #[test]
    fn selected_source_without_admitted_physical_proof_cannot_take_hot_route() {
        let mut engine = engine();
        let old = initial(&mut engine);
        assert!(
            engine
                .model()
                .active()
                .unwrap()
                .applied()
                .physical_node()
                .is_none()
        );
        let binding = (
            std::path::Path::new("/sys/selected/video"),
            std::path::Path::new("/sys/selected"),
        );
        assert!(matches!(
            engine
                .apply_selected_capture(
                    engine.model().state_identity(),
                    engine.model().draft().revision,
                    binding,
                )
                .unwrap(),
            ApplyAdmission::Open { .. }
        ));
        assert_eq!(engine.model().active().unwrap().attempt(), old);
        assert!(engine.runner.treatment_intents.is_empty());
        assert!(engine.runner.stops.is_empty());
        assert_eq!(
            engine
                .model()
                .validation_request()
                .unwrap()
                .pinned_node
                .as_ref()
                .map(|(node, usb)| (node.as_path(), usb.as_path())),
            Some(binding)
        );
    }

    #[test]
    fn session_node_binding_reopens_cli_node_across_restart_resume_recovery_and_reconnect() {
        for operation in 0..4 {
            let mut engine = native_node_engine();
            let old = initial(&mut engine);
            assert_eq!(engine.model().phase(), ProductPhase::Active);
            match operation {
                0 => {
                    engine.restart(engine.model().state_identity()).unwrap();
                }
                1 => {
                    confirm_pause(&mut engine, old);
                    engine.resume(engine.model().state_identity(), old).unwrap();
                }
                2 | 3 => {
                    eof(&mut engine, old);
                    engine.runner.barrier(old);
                    engine.poll();
                    if operation == 3 {
                        engine.validator.native_snapshot =
                            Some(crate::capture::linux::session_fixture(
                                &["/dev/video1"],
                                engine
                                    .model()
                                    .last_valid()
                                    .unwrap()
                                    .settings()
                                    .video
                                    .fixture_mode(),
                            ));
                        validate(&mut engine);
                        assert!(engine.model().active().is_none());
                        engine.validator.native_snapshot =
                            Some(crate::capture::linux::session_fixture(
                                &["/dev/video0", "/dev/video1"],
                                engine
                                    .model()
                                    .last_valid()
                                    .unwrap()
                                    .settings()
                                    .video
                                    .fixture_mode(),
                            ));
                        engine.reconnect(engine.model().state_identity()).unwrap();
                    }
                }
                _ => unreachable!(),
            }
            validate(&mut engine);
            if operation < 2 {
                engine.runner.barrier(old);
                engine.poll();
            }
            engine.runner.verified();
            engine.poll();
            assert_eq!(
                engine.model().phase(),
                ProductPhase::Active,
                "operation {operation}"
            );
            assert_eq!(
                engine
                    .model()
                    .active()
                    .unwrap()
                    .applied()
                    .settings()
                    .video
                    .resolved_fourcc,
                Some(CapturedFourCc::from_bytes(*b"YUYV"))
            );
        }
    }

    #[test]
    fn session_node_binding_refuses_sibling_when_admitted_node_disappears() {
        for operation in 0..4 {
            let mut engine = native_node_engine();
            let old = initial(&mut engine);
            engine.validator.native_snapshot = Some(crate::capture::linux::session_fixture(
                &["/dev/video1"],
                engine
                    .model()
                    .last_valid()
                    .unwrap()
                    .settings()
                    .video
                    .fixture_mode(),
            ));
            match operation {
                0 => {
                    engine.restart(engine.model().state_identity()).unwrap();
                }
                1 => {
                    confirm_pause(&mut engine, old);
                    engine.resume(engine.model().state_identity(), old).unwrap();
                }
                2 | 3 => {
                    eof(&mut engine, old);
                    engine.runner.barrier(old);
                    engine.poll();
                    if operation == 3 {
                        validate(&mut engine);
                        assert!(engine.model().active().is_none());
                        engine.reconnect(engine.model().state_identity()).unwrap();
                    }
                }
                _ => unreachable!(),
            }
            validate(&mut engine);
            assert_eq!(
                engine.runner.opens.len(),
                1,
                "sibling must not open for operation {operation}"
            );
            assert!(engine.model().opening().is_none());
            let rejection = engine.model().validation_rejection().unwrap_or_else(|| {
                panic!(
                    "missing bound node must report validation refusal for operation {operation}, phase {:?}",
                    engine.model().phase()
                )
            });
            assert_eq!(rejection.failure.cause, Cause::RequestedModeRefused);
            assert!(!rejection.failure.diagnostic.is_empty());
            assert_eq!(
                rejection.request.pinned_node,
                Some(("/sys/dev/video0".into(), "/sys/usb/fixture".into()))
            );
            if operation < 2 {
                let active = engine
                    .model()
                    .active()
                    .expect("prevalidation keeps incumbent");
                assert_eq!(active.attempt(), old);
                assert_eq!(
                    active.playback(),
                    if operation == 1 {
                        PlaybackState::Paused
                    } else {
                        PlaybackState::Live
                    }
                );
                assert!(engine.runner.stops.is_empty());
            } else {
                assert!(engine.model().active().is_none());
                assert!(engine.model().failures().is_some());
            }
        }
    }

    #[test]
    fn session_node_binding_accepts_devnode_renumbering_with_same_physical_syspath() {
        let mut engine = native_node_engine();
        let old = initial(&mut engine);
        let applied = engine.model().last_valid().unwrap().settings();
        engine.validator.native_snapshot = Some(crate::capture::linux::physical_fixture(&[(
            applied.video.identity.clone(),
            "/dev/video9",
            "/sys/usb/fixture",
            "/sys/dev/video0",
            applied.video.fixture_mode(),
        )]));
        engine.restart(engine.model().state_identity()).unwrap();
        validate(&mut engine);
        engine.runner.barrier(old);
        engine.poll();
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Active);
    }

    #[test]
    fn session_node_binding_survives_close_but_is_absent_in_new_model_and_serialized_applied() {
        let mut engine = native_node_engine();
        let old = initial(&mut engine);
        let applied = engine.model().last_valid().unwrap();
        let serialized = serde_json::to_value(applied).unwrap();
        assert!(serialized.get("pinned_node").is_none());
        let mut fresh = ProductModel::new(applied.settings().clone());
        fresh
            .apply(fresh.state_identity(), fresh.draft().revision)
            .unwrap();
        assert!(fresh.validation_request().unwrap().pinned_node.is_none());
        engine.close(engine.model().state_identity()).unwrap();
        engine.runner.barrier(old);
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Stopped);
        engine.restart(engine.model().state_identity()).unwrap();
        validate(&mut engine);
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Active);
    }

    #[test]
    fn session_node_binding_survives_hot_treatment_commit_and_fresh_owner_restore() {
        for restore in [false, true] {
            let mut engine = native_node_engine();
            let old = initial(&mut engine);
            engine.take_verified_applied();
            let mut candidate = engine.model().draft().settings.clone();
            candidate.renderer = altered_renderer(&candidate.renderer);
            let revision = engine
                .edit_draft(engine.model().draft().revision, candidate)
                .unwrap();
            let ApplyAdmission::Treatments { key, .. } = engine
                .apply(engine.model().state_identity(), revision)
                .unwrap()
            else {
                panic!("same-source renderer edit uses hot treatment");
            };
            treatment_success(&mut engine, key);
            engine.poll();
            if restore {
                engine
                    .runner
                    .events
                    .push_back(SessionEvent::TreatmentFault {
                        key,
                        failure: applied_treatment_fault(key),
                    });
                engine.poll();
                assert_eq!(
                    engine.model().treating().unwrap().route(),
                    Some(TreatmentRestoreRoute::FreshOwner)
                );
                engine.runner.barrier(old);
                engine.poll();
            } else {
                engine.restart(engine.model().state_identity()).unwrap();
            }
            validate(&mut engine);
            if !restore {
                engine.runner.barrier(old);
                engine.poll();
            }
            engine.runner.verified();
            engine.poll();
            assert!(engine.model().active().is_some());
            assert_eq!(engine.runner.opens.len(), 2);
        }
    }

    #[test]
    fn session_node_binding_new_candidate_replaces_pin_but_failed_candidate_restores_prior_pin() {
        for fail in [false, true] {
            let mut engine = native_node_engine();
            let old = initial(&mut engine);
            let prior = engine.model().last_valid().unwrap().settings().clone();
            let mut candidate = prior.clone();
            candidate.video.mode.rate = FrameRate::new(30, 1).unwrap();
            candidate.video.selected_node = Some("/dev/video1".into());
            candidate.video.resolved_fourcc = None;
            engine.validator.native_snapshot = Some(crate::capture::linux::session_fixture(
                &["/dev/video0", "/dev/video1"],
                candidate
                    .video
                    .mode
                    .with_fourcc(CapturedFourCc::from_bytes(*b"YUYV")),
            ));
            engine
                .edit_draft(engine.model().draft().revision, candidate)
                .unwrap();
            apply(&mut engine);
            assert!(
                engine
                    .model()
                    .validation_request()
                    .unwrap()
                    .pinned_node
                    .is_none()
            );
            validate(&mut engine);
            engine.runner.barrier(old);
            engine.poll();
            let next = engine.runner.opens.last().unwrap().0.attempt;
            if fail {
                engine.runner.fail("new candidate failed");
                engine.poll();
                engine.validator.native_snapshot = Some(crate::capture::linux::session_fixture(
                    &["/dev/video0", "/dev/video1"],
                    prior.video.fixture_mode(),
                ));
                engine.runner.barrier(next);
                engine.poll();
                validate(&mut engine);
            }
            engine.runner.verified();
            engine.poll();
            assert!(engine.model().active().is_some());
            let incumbent = engine.model().active().unwrap().attempt();
            engine.restart(engine.model().state_identity()).unwrap();
            assert_eq!(
                engine.model().validation_request().unwrap().pinned_node,
                Some((
                    if fail {
                        "/sys/dev/video0".into()
                    } else {
                        "/sys/dev/video1".into()
                    },
                    "/sys/usb/fixture".into(),
                ))
            );
            validate(&mut engine);
            engine.runner.barrier(incumbent);
            engine.poll();
            engine.runner.verified();
            engine.poll();
            assert!(engine.model().active().is_some());
        }
    }

    #[test]
    fn resolved_auto_open_commits_concrete_watch_and_requires_fresh_observation() {
        use crate::domain::capture::InputFormatPolicy;
        let mut draft = settings(60);
        draft.video.mode.format_policy = InputFormatPolicy::Auto;
        draft.video.resolved_fourcc = None;
        draft.video.selected_node = Some("/dev/video0".into());
        let mut engine = ApplyCoordinator::new(
            draft,
            PlaybackGain::default(),
            Validator::default(),
            Runner::default(),
        );
        apply(&mut engine);
        let target = engine.model().watch_target().unwrap().clone();
        assert_eq!(engine.installed_watch.as_ref(), Some(&target));
        let request = engine.validator.request.take().unwrap();
        let stamp = engine.validator.observed.unwrap_or(request.watch);
        let mut prepared = request.settings.clone();
        prepared.video.resolved_fourcc = Some(CapturedFourCc::from_bytes(*b"YUYV"));
        engine.validator.result = Some(ValidationResult {
            stamp,
            result: ValidationOutcome::Prepared(TestPrepared {
                settings: prepared.clone(),
                stamp,
                node: None,
            }),
            request,
        });
        engine.poll();
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.model().phase(), ProductPhase::Active);
        assert_eq!(engine.model().last_valid().unwrap().settings(), &prepared);
        let committed = engine.model().watch_target().unwrap().clone();
        assert_ne!(committed.watch, target.watch);
        assert_eq!(committed.video, prepared.video);
        assert_eq!(engine.installed_watch.as_ref(), Some(&committed));
        assert_eq!(engine.lease.as_ref().unwrap().watch, stamp);
        assert!(engine.model().observation().is_none());
        let fresh = engine.validator.observation.take().unwrap();
        let mut stale = fresh.clone();
        stale.stamp = stamp;
        engine.validator.observation = Some(stale);
        engine.poll();
        assert!(engine.model().observation().is_none());
        engine.validator.observation = Some(fresh.clone());
        engine.poll();
        assert_eq!(engine.model().observation(), Some(&fresh));
        assert_eq!(fresh.stamp.watch, committed.watch);
        assert_eq!(engine.model().last_valid().unwrap().settings(), &prepared);
        assert_eq!(engine.runner.opens.len(), 1);
    }

    #[test]
    fn verified_applied_uses_exact_committed_settings_and_is_consumed_once() {
        let mut engine = engine();
        let apply = apply(&mut engine);
        assert!(engine.take_verified_applied().is_none());
        validate(&mut engine);
        assert!(engine.take_verified_applied().is_none());
        let key = engine.runner.opens.last().unwrap().0;
        engine
            .edit_draft(engine.model().draft().revision, settings(30))
            .unwrap();
        // Both coalesced and later duplicate receipts must produce one event.
        engine.runner.verified();
        engine.runner.verified();
        engine.poll();
        assert_eq!(engine.installed_watch.as_ref(), engine.model.watch_target());
        let VerifiedApplied::Open {
            key: received,
            applied,
        } = engine.take_verified_applied().unwrap()
        else {
            panic!("user Open result")
        };
        assert_eq!(received, key);
        assert_eq!(received.apply, apply);
        assert_eq!(applied.settings(), &settings(60));
        assert!(engine.model.draft().settings.same_request(&settings(30)));
        assert!(engine.take_verified_applied().is_none());
        engine.runner.verified();
        engine.poll();
        assert!(engine.take_verified_applied().is_none());
    }

    #[test]
    fn stale_or_wrong_receipts_never_publish_verified_applied() {
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
                receipt: Box::new(receipt(settings(60), stale)),
            });
            engine.poll();
            assert_eq!(engine.model.opening().unwrap().0, key);
            assert!(engine.take_verified_applied().is_none());
        }
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(receipt(settings(30), key)),
        });
        engine.poll();
        assert!(engine.model.active().is_none());
        assert!(engine.model.last_valid().is_none());
        assert!(engine.take_verified_applied().is_none());
        engine.runner.verified();
        engine.poll();
        assert!(engine.take_verified_applied().is_none());
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
            assert!(engine.take_verified_applied().is_none());
        }
    }

    #[test]
    fn verified_applied_requires_successful_committed_watch_installation() {
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
                assert!(engine.take_verified_applied().is_none());
                let context = engine.diagnostic_failures().incumbent.as_ref().unwrap();
                assert_eq!(context.identity.phase(), ProductPhase::Active);
                assert_eq!(context.error.operation, "committed_watch");
                assert_eq!(
                    context.correlation,
                    crate::app::diagnostic::DiagnosticFailureCorrelation::Owner {
                        attempt: key.attempt,
                        audio_epoch: None,
                    }
                );
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
                let VerifiedApplied::Open {
                    key: received,
                    applied,
                } = engine.take_verified_applied().unwrap()
                else {
                    panic!("user Open result")
                };
                assert_eq!(received, key);
                assert_eq!(applied.settings(), &relocated);
                assert!(engine.take_verified_applied().is_none());
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
            let admitted = apply(&mut engine);
            if strict {
                engine.require_startup_restore_audio(admitted).unwrap();
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
            let mut opened = receipt(desired.clone(), key);
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
            assert_eq!(engine.take_verified_applied().is_some(), !strict);
            assert!(engine.strict_restore_apply.is_none());
            if strict {
                assert_eq!(engine.runner.stops, vec![(key.attempt, StopReason::Failed)]);
                engine.runner.barrier(key.attempt);
                engine.poll();
                assert_eq!(engine.model.phase(), ProductPhase::ErrorWithoutActive);
                assert_eq!(engine.runner.opens.len(), 1);
                assert!(engine.model.draft().settings.same_request(&desired));
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
        let mut opened = receipt(desired.clone(), key);
        opened.audio = AudioOutcome::Active { route };
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(opened),
        });
        engine.poll();
        let VerifiedApplied::Open {
            key: received,
            applied,
        } = engine.take_verified_applied().unwrap()
        else {
            panic!("startup Open result")
        };
        assert_eq!(received, key);
        assert_eq!(applied.settings(), &desired);
        assert!(engine.strict_restore_apply.is_none());
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
        assert!(engine.take_verified_applied().is_none());
    }

    #[test]
    fn profile_restore_guard_keeps_strict_audio_across_incumbent_replacement_window() {
        use crate::domain::capture::{AudioError, AudioSourceIdentity, VideoPresence};
        let source = AudioSourceIdentity::new("exact.capture".into(), vec![]).unwrap();
        let mut engine = engine();
        let _incumbent = initial(&mut engine);
        let mut replacement = settings(30);
        replacement.audio = AudioSelection::Enabled {
            source: source.clone(),
        };
        engine
            .edit_draft(engine.model().draft().revision, replacement.clone())
            .unwrap();
        let admitted = apply(&mut engine);
        engine.require_profile_restore_audio(admitted).unwrap();
        // The strict verdict keys the CURRENT observation: push the absent
        // source before validation so the receipt is judged against epoch 2.
        let absent = observation(
            &engine,
            2,
            VideoPresence::Present,
            SourcePresence::Absent(AudioError::Cancelled),
            None,
        );
        observe(&mut engine, absent);
        validate(&mut engine);
        // The old owner replacement window has neither a pending validation
        // request nor an opening key: the guard must survive until rollback.
        assert!(
            engine.model().validation_request().is_none() && engine.model().opening().is_none()
        );
        assert_eq!(engine.strict_restore_apply, Some(admitted));
        // The runner still holds the incumbent's opening key: retire that old
        // owner through its replacement barrier before B actually opens, the
        // same deterministic sequencing the switch fixtures use.
        let old_key = engine.runner.opens.last().unwrap().0;
        assert_ne!(old_key.apply, admitted);
        engine.runner.barrier(old_key.attempt);
        engine.poll();
        let key = engine.runner.opens.last().unwrap().0;
        assert_eq!(key.apply, admitted);
        assert_ne!(key.attempt, old_key.attempt);
        assert_eq!(engine.strict_restore_apply, Some(admitted));
        let mut opened = receipt(replacement.clone(), key);
        opened.audio = AudioOutcome::Silent {
            source,
            reason: crate::domain::capture::AudioSilence::WaitingForSource(AudioError::Cancelled),
        };
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(opened),
        });
        engine.poll();
        assert_eq!(engine.strict_restore_apply, None);
        assert!(engine.take_verified_applied().is_none());
        // A's physical owner was retired before B opened. Rejection starts
        // rollback; no active stream can exist until B's own shutdown barrier
        // and fresh validation of the prior settings have both completed.
        assert!(engine.model().active().is_none());
        assert!(engine.model().failures().unwrap().candidate.is_some());
        assert!(engine.model().draft().settings.same_request(&replacement));
        engine.runner.barrier(key.attempt);
        engine.poll();
        let restore = engine.model().validation_request().unwrap();
        assert_eq!(restore.key.purpose, AttemptPurpose::Restore);
        assert!(restore.settings.same_request(&settings(60)));
        validate(&mut engine);
        let restore_key = engine.runner.opens.last().unwrap().0;
        assert_eq!(restore_key.purpose, AttemptPurpose::Restore);
        assert_ne!(restore_key.attempt, _incumbent);
        engine.runner.verified();
        engine.poll();
        let active = engine.model().active().expect("prior settings restored");
        assert!(active.applied().settings().same_request(&settings(60)));
        assert!(engine.model().draft().settings.same_request(&replacement));
        assert!(engine.take_verified_applied().is_none());
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
            assert_eq!(engine.strict_restore_apply, Some(id));
            if canceled {
                engine.close(engine.model.state_identity()).unwrap();
                assert!(engine.strict_restore_apply.is_none());
                validate(&mut engine);
            } else {
                engine.validator.finish(Err(failure(
                    settings(60),
                    FailureCategory::Validation(ValidationLayer::Mode),
                    "startup validation refused",
                )));
                engine.poll();
            }
            assert!(engine.strict_restore_apply.is_none());
            assert!(engine.take_verified_applied().is_none());
            let next = apply(&mut engine);
            assert_ne!(next, id);
            validate(&mut engine);
            engine.runner.verified();
            engine.poll();
            assert!(
                matches!(engine.take_verified_applied(), Some(VerifiedApplied::Open { key, .. }) if key.apply == next)
            );
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
        assert!(engine.model().draft().settings.same_request(&settings(30)));
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
    fn explicit_pause_rejected_admission_keeps_actual_same_source_and_explicit_renderer_apply_eligible()
     {
        for status in [
            SubmitStatus::NotReady,
            SubmitStatus::Closing,
            SubmitStatus::CapacityExceeded,
            SubmitStatus::StaleGeneration,
        ] {
            for explicit in [false, true] {
                let mut engine = engine();
                let current = initial(&mut engine);
                let before = engine.model().state_identity();
                engine.runner.immediate = Some(status);
                assert_eq!(engine.pause(current), Ok(status));
                assert_eq!(engine.model().state_identity(), before);
                let ImmediateIntent::SetPaused { request, .. } =
                    &engine.runner.intents.last().unwrap().1
                else {
                    panic!("pause reservation")
                };
                let request = *request;
                assert!(
                    !engine.model.pause_admitted(current, request),
                    "a refused old reservation cannot become pending"
                );
                let candidate = renderer_settings(38);
                let revision = engine
                    .edit_draft(engine.model().draft().revision, candidate.clone())
                    .unwrap();
                let admission = if explicit {
                    engine
                        .apply_treatments(engine.model().state_identity(), revision)
                        .unwrap()
                } else {
                    engine
                        .apply(engine.model().state_identity(), revision)
                        .unwrap()
                };
                let ApplyAdmission::Treatments { key, .. } = admission else {
                    panic!("LIVE renderer admission")
                };
                treatment_success(&mut engine, key);
                engine.poll();
                assert_eq!(
                    engine.model().active().unwrap().applied().settings(),
                    &candidate
                );
                assert_eq!(
                    engine.model().active().unwrap().playback(),
                    PlaybackState::Live
                );
                assert_eq!(engine.runner.opens.len(), 1);
                assert!(engine.runner.stops.is_empty());
                assert_eq!(engine.pause(current), Ok(SubmitStatus::Accepted));
                assert!(matches!(
                    engine.model().active().unwrap().playback(),
                    PlaybackState::PausePending { .. }
                ));
            }
        }
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
                assert!(
                    engine
                        .validator
                        .requests
                        .last()
                        .unwrap()
                        .settings
                        .same_request(&target)
                );
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
        assert!(engine.model().draft().settings.same_request(&settings(30)));
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
            ValidationLayer::Renderer,
            ValidationLayer::Renderer,
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
            assert!(engine.model().draft().settings.same_request(&settings(30)));
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
        assert!(engine.model().draft().settings.same_request(&settings(30)));
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
        assert!(engine.model().draft().settings.same_request(&settings(30)));
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
                receipt: Box::new(receipt(settings(60), key)),
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
        assert!(engine.model().draft().settings.same_request(&settings(25)));
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
                    let stale = receipt(lease.settings.clone(), key);
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
                assert!(engine.model().draft().settings.same_request(&settings(30)));
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
                receipt: Box::new(receipt(settings(30), candidate)),
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
            let mut opened = receipt(selected, key);
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
        let mut opened = receipt(settings(60), key);
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
            engine
                .edit_draft(engine.model().draft().revision, settings(30))
                .unwrap();
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
                node: None,
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
    fn terminal_and_cleanup_blockers_preserve_causes_before_retirement_in_either_order() {
        for blocker_first in [false, true] {
            for retire_in_drain in [false, true] {
                let mut engine = engine();
                let old = initial(&mut engine);
                engine.take_verified_applied();
                let terminal = failure(
                    settings(60),
                    FailureCategory::Lifecycle(LifecycleFailure::SurfaceLoss),
                    "surface lost",
                );
                let poison = failure(
                    settings(60),
                    FailureCategory::Lifecycle(LifecycleFailure::SurfaceLoss),
                    "native lifetime poisoned",
                );
                let acknowledgement = failure(
                    settings(60),
                    FailureCategory::Lifecycle(LifecycleFailure::Acknowledgement),
                    "missing acknowledgement",
                );
                if blocker_first {
                    engine
                        .runner
                        .events
                        .push_back(SessionEvent::CleanupBlocked {
                            attempt: old,
                            failure: poison.clone(),
                        });
                }
                engine.runner.events.push_back(SessionEvent::SessionFailed {
                    attempt: old,
                    failure: terminal.clone(),
                });
                if !blocker_first {
                    engine
                        .runner
                        .events
                        .push_back(SessionEvent::CleanupBlocked {
                            attempt: old,
                            failure: poison.clone(),
                        });
                }
                for _ in 0..2 {
                    engine
                        .runner
                        .events
                        .push_back(SessionEvent::CleanupBlocked {
                            attempt: old,
                            failure: acknowledgement.clone(),
                        });
                }
                if retire_in_drain {
                    engine.runner.barrier(old);
                }
                engine.runner.verified(); // A cached positive cannot cross terminal/blocker facts.
                engine.poll();
                assert!(engine.model().active().is_none());
                assert_eq!(
                    engine.model().cleanup(),
                    &CleanupStatus::Blocked {
                        failure: poison.clone()
                    }
                );
                let report = engine.model().failures().unwrap();
                assert_eq!(report.incumbent.as_ref(), Some(&terminal));
                assert_eq!(
                    report.cleanup,
                    vec![poison.clone(), acknowledgement.clone()]
                );
                assert_eq!(engine.runner.opens.len(), 1);
                assert_eq!(engine.validator.requests.len(), 1);
                assert!(engine.model().opening().is_none());
                assert!(engine.take_verified_applied().is_none());
                assert_eq!(
                    engine.reconnect(engine.model().state_identity()),
                    Err(CommandRejection::CleanupBlocked)
                );
                engine.quit();
                engine.validator.retired = true;
                engine.poll();
                if !retire_in_drain {
                    assert!(!engine.model().shutdown_ready());
                    engine.runner.barrier(old);
                    engine.poll();
                }
                assert!(engine.model().shutdown_ready());
                let report = engine.model().failures().unwrap();
                assert_eq!(report.incumbent.as_ref(), Some(&terminal));
                assert_eq!(report.cleanup, vec![poison, acknowledgement]);
                assert_eq!(
                    engine.runner.opens.len(),
                    1,
                    "blocked lifetime cannot reopen during retirement or Quit"
                );
            }
        }
    }
    #[test]
    fn opening_failure_blockers_fence_resource_free_and_same_drain_retirement_before_restore() {
        for blocker_first in [false, true] {
            for resource_free_stop in [false, true] {
                let mut engine = engine();
                let old = initial(&mut engine);
                engine.take_verified_applied();
                engine
                    .edit_draft(engine.model().draft().revision, settings(30))
                    .unwrap();
                let admission = engine
                    .apply(
                        engine.model().state_identity(),
                        engine.model().draft().revision,
                    )
                    .unwrap();
                validate(&mut engine);
                engine.runner.barrier(old);
                engine.poll();
                let candidate = engine.model().opening().unwrap().0;
                assert_eq!(candidate.purpose, AttemptPurpose::Candidate);
                let rejected = failure(
                    settings(30),
                    FailureCategory::Session,
                    "source candidate failed",
                );
                let poison = failure(
                    settings(30),
                    FailureCategory::Lifecycle(LifecycleFailure::SurfaceLoss),
                    "candidate native lifetime poisoned",
                );
                if resource_free_stop {
                    engine.runner.stop_failure = Some(StopSubmission::NoResourcesCreated);
                }
                if blocker_first {
                    engine
                        .runner
                        .events
                        .push_back(SessionEvent::CleanupBlocked {
                            attempt: candidate.attempt,
                            failure: poison.clone(),
                        });
                }
                engine.runner.events.push_back(SessionEvent::OpenFailed {
                    key: candidate,
                    failure: rejected.clone(),
                });
                if !blocker_first {
                    engine
                        .runner
                        .events
                        .push_back(SessionEvent::CleanupBlocked {
                            attempt: candidate.attempt,
                            failure: poison.clone(),
                        });
                }
                if !resource_free_stop {
                    engine.runner.barrier(candidate.attempt);
                }
                engine.runner.verified();
                engine.poll();
                assert_eq!(engine.model().phase(), ProductPhase::ErrorWithoutActive);
                assert!(engine.model().active().is_none());
                assert!(engine.model().opening().is_none());
                assert_eq!(
                    engine.model().cleanup(),
                    &CleanupStatus::Blocked {
                        failure: poison.clone()
                    }
                );
                assert_eq!(
                    engine.model().failures().unwrap().candidate.as_ref(),
                    Some(&rejected)
                );
                assert_eq!(
                    engine.model().failures().unwrap().cleanup,
                    vec![poison.clone()]
                );
                assert_eq!(
                    engine.runner.opens.len(),
                    2,
                    "no Restore owner may cross a cleanup blocker"
                );
                assert_eq!(
                    engine.validator.requests.len(),
                    2,
                    "even resource-free retirement cannot admit Restore validation"
                );
                assert!(
                    engine
                        .validator
                        .requests
                        .iter()
                        .all(|request| request.key.purpose != AttemptPurpose::Restore)
                );
                assert!(!engine.has_user_apply_result_or_pending(admission.id()));
                assert!(engine.take_verified_applied().is_none());
                engine.quit();
                engine.validator.retired = true;
                engine.poll();
                assert!(engine.model().shutdown_ready());
                assert_eq!(
                    engine.model().failures().unwrap().candidate.as_ref(),
                    Some(&rejected)
                );
                assert_eq!(engine.model().failures().unwrap().cleanup, vec![poison]);
                assert_eq!(engine.runner.opens.len(), 2);
            }
        }
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
        assert!(engine.model().draft().settings.same_request(&settings(30)));
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
            receipt: Box::new(receipt(settings(60), first)),
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
                node: None,
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
            receipt: Box::new(receipt(settings(60), key)),
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
        assert!(engine.model().draft().settings.same_request(&settings(30)));
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
        let mut receipt = receipt(desired, key);
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
        let context = engine.diagnostic_failures().incumbent.as_ref().unwrap();
        assert_eq!(context.identity.phase(), ProductPhase::Active);
        assert_eq!(context.error.operation, "audio_detach");
        assert_eq!(
            context.correlation,
            crate::app::diagnostic::DiagnosticFailureCorrelation::Owner {
                attempt,
                audio_epoch: Some(AudioEpoch::new(1).unwrap()),
            }
        );
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
        assert!(request.settings.same_request(&settings(30)));
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
        let context = engine.diagnostic_failures().validation.as_ref().unwrap();
        assert_eq!(context.identity.phase(), ProductPhase::Validating);
        assert_eq!(
            context.error.requested.mode.rate,
            FrameRate::new(30, 1).unwrap()
        );
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
        let mut ready = receipt(desired, key);
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
                node: None,
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
        let mut ready = receipt(desired, key);
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
                node: None,
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
        let mut opened = receipt(desired, key);
        opened.audio = AudioOutcome::Silent {
            source: source.clone(),
            reason: AudioSilence::Output(OutputSilence::ManualRequiresAction),
        };
        engine.runner.events.push_back(SessionEvent::OpenVerified {
            key,
            receipt: Box::new(opened),
        });
        engine.poll();
        // Settle the authoritative observation for the post-open committed watch.
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
        // The mock route carries the exact current observation admitted with the
        // AttachAudio authorization (lease.audio_stamp), not the open attempt's
        // original capture watch.
        let admitted = engine
            .lease
            .as_ref()
            .unwrap()
            .audio_stamp
            .expect("admitted attach audio observation stamp");
        assert!(
            matches!(&engine.runner.intents[1], (active, ImmediateIntent::AttachAudio { epoch: epoch_value, source: selected, stamp })
            if *active == attempt && *epoch_value == epoch && selected == &source && *stamp == admitted)
        );
        let route = crate::media::loopback::LoopbackReceipt::for_test(
            crate::media::controller::Generation::new(attempt.get()).unwrap(),
            attempt,
            epoch,
            admitted,
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
        assert!(engine.model().draft().settings.same_request(&dirty));
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
        assert!(engine.model().draft().settings.same_request(&dirty));
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
        // The mock route must carry the exact current observation admitted with
        // the AttachAudio authorization (lease.audio_stamp), not the open
        // attempt's original capture watch.
        let admitted = engine
            .lease
            .as_ref()
            .unwrap()
            .audio_stamp
            .expect("admitted attach audio observation stamp");
        let route = crate::media::loopback::LoopbackReceipt::for_test(
            crate::media::controller::Generation::new(attempt.get()).unwrap(),
            attempt,
            epoch,
            admitted,
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
        // Consumer-visible proof that the fresh admitted route is accepted while
        // the live epoch is still kept before any silence transition.
        assert!(matches!(
            engine.audio_availability(),
            Some(AudioAvailability::Active { .. })
        ));
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
            admitted,
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
        let mut opened = receipt(desired, key);
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
        assert!(engine.take_verified_applied().is_none());
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

    #[test]
    fn diagnostic_equal_failures_follow_new_admissions_not_error_equality() {
        use crate::app::diagnostic::DiagnosticFailureCorrelation;
        let mut engine = engine();
        let error = failure(
            settings(60),
            FailureCategory::Validation(ValidationLayer::Mode),
            "same",
        );
        let mut previous = None;
        for _ in 0..2 {
            apply(&mut engine);
            let request = engine.validator.request.clone().unwrap();
            let identity = engine.model.state_identity();
            engine.validator.finish(Err(error.clone()));
            engine.poll();
            let context = engine.diagnostic_failures().validation.as_ref().unwrap();
            assert_eq!(context.identity, identity);
            assert_eq!(
                context.correlation,
                DiagnosticFailureCorrelation::Validation {
                    key: request.key,
                    revision: request.revision,
                    watch: request.watch,
                }
            );
            assert_ne!(previous, Some(context.correlation));
            previous = Some(context.correlation);
            assert!(engine.diagnostic_failures().candidate.is_none());
        }
    }

    #[test]
    fn diagnostic_hidden_restored_origin_returns_after_validation_rejection() {
        let mut engine = reach_step(5);
        engine.runner.verified();
        engine.poll();
        let original = engine.diagnostic_failures().candidate.clone().unwrap();
        engine
            .edit_draft(engine.model.draft().revision, settings(24))
            .unwrap();
        apply(&mut engine);
        assert!(engine.diagnostic_failures().candidate.is_none());
        engine.validator.finish(Err(failure(
            settings(24),
            FailureCategory::Validation(ValidationLayer::Mode),
            "rejected",
        )));
        engine.poll();
        assert_eq!(
            engine
                .diagnostic_failures()
                .candidate
                .as_ref()
                .unwrap()
                .as_ref(),
            original.as_ref()
        );
        assert_eq!(engine.model.phase(), ProductPhase::ErrorWithActiveRestored);
        assert_eq!(
            engine
                .diagnostic_failures()
                .validation
                .as_ref()
                .unwrap()
                .identity
                .phase(),
            ProductPhase::Validating
        );
    }

    #[test]
    fn diagnostic_error_without_active_retains_both_lanes_after_new_validation_rejection() {
        let mut engine = reach_step(6);
        let restore = engine.lease.as_ref().unwrap().key.attempt;
        engine.runner.barrier(restore);
        engine.poll();
        let original = engine.diagnostic_failures().clone();
        assert!(original.candidate.is_some() && original.restore.is_some());
        engine
            .edit_draft(engine.model.draft().revision, settings(24))
            .unwrap();
        apply(&mut engine);
        assert!(engine.diagnostic_failures().candidate.is_none());
        assert!(engine.model.validation_request().is_some());
        engine.validator.finish(Err(failure(
            settings(24),
            FailureCategory::Validation(ValidationLayer::Mode),
            "new validation rejected",
        )));
        engine.poll();
        assert_eq!(engine.model.phase(), ProductPhase::ErrorWithoutActive);
        assert_eq!(engine.diagnostic_failures().candidate, original.candidate);
        assert_eq!(engine.diagnostic_failures().restore, original.restore);
        assert_eq!(
            engine
                .diagnostic_failures()
                .validation
                .as_ref()
                .unwrap()
                .identity
                .phase(),
            ProductPhase::Validating
        );
    }

    #[test]
    fn diagnostic_candidate_and_restore_retain_distinct_pre_failure_phases() {
        for success in [false, true] {
            let mut engine = reach_step(5);
            let candidate = engine.diagnostic_failures().candidate.clone().unwrap();
            assert_eq!(candidate.identity.phase(), ProductPhase::OpeningCandidate);
            assert_eq!(
                candidate.error.requested.mode.rate,
                FrameRate::new(30, 1).unwrap()
            );
            if success {
                engine.runner.verified();
            } else {
                engine.runner.fail("restore rejected");
            }
            engine.poll();
            assert_eq!(
                engine
                    .diagnostic_failures()
                    .candidate
                    .as_ref()
                    .unwrap()
                    .as_ref(),
                candidate.as_ref()
            );
            if success {
                assert!(engine.diagnostic_failures().restore.is_none());
                assert_eq!(engine.model.phase(), ProductPhase::ErrorWithActiveRestored);
            } else {
                let restore = engine.diagnostic_failures().restore.as_ref().unwrap();
                assert_eq!(restore.identity.phase(), ProductPhase::OpeningRestore);
                assert_eq!(
                    restore.error.requested.mode.rate,
                    FrameRate::new(60, 1).unwrap()
                );
                assert_ne!(restore.correlation, candidate.correlation);
            }
        }
    }

    #[test]
    fn diagnostic_late_loss_evidence_enriches_original_identity_and_ignores_stale_fault() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let confirmed = engine.model.confirmed_treatment_key().unwrap();
        engine.runner.events.push_back(SessionEvent::StreamEnded {
            attempt: old,
            reason: 4,
            error: -1,
        });
        engine.poll();
        let original = engine.diagnostic_failures().incumbent.clone().unwrap();
        engine
            .runner
            .events
            .push_back(SessionEvent::TreatmentFault {
                key: confirmed,
                failure: treatment_error(
                    confirmed,
                    TreatmentErrorKind::Unconfirmed {
                        reason: TreatmentConfirmationFailure::BackendUnavailable,
                    },
                    true,
                ),
            });
        engine.poll();
        let enriched = engine.diagnostic_failures().incumbent.clone().unwrap();
        assert_eq!(enriched.identity, original.identity);
        assert_eq!(enriched.correlation, original.correlation);
        assert!(enriched.error.treatment.is_some());
        let stale = TreatmentAttemptKey {
            apply: ApplyId::new(999).unwrap(),
            ..confirmed
        };
        engine
            .runner
            .events
            .push_back(SessionEvent::TreatmentFault {
                key: stale,
                failure: treatment_error(stale, deadline(), true),
            });
        engine.poll();
        assert_eq!(
            engine
                .diagnostic_failures()
                .incumbent
                .as_ref()
                .unwrap()
                .as_ref(),
            enriched.as_ref()
        );
    }

    #[test]
    fn diagnostic_cleanup_keeps_six_original_indices_through_deduplication_and_close() {
        let mut engine = reach_step(3);
        let key = engine.lease.as_ref().unwrap().key;
        let candidate = engine.diagnostic_failures().candidate.clone().unwrap();
        let mut first = None;
        for index in 0..8 {
            let mut error = failure(
                settings(30),
                FailureCategory::Lifecycle(LifecycleFailure::Quiescence),
                "private",
            );
            error.evidence = Some(crate::domain::failure::BackendEvidence {
                operation: crate::domain::failure::BackendOperation::Unknown,
                errno: index,
            });
            if index == 0 {
                first = Some(error.clone());
            }
            engine.handle_event(SessionEvent::CleanupBlocked {
                attempt: key.attempt,
                failure: error,
            });
        }
        let retained = engine.diagnostic_failures().clone();
        assert_eq!(retained.cleanup.iter().flatten().count(), 6);
        for (index, context) in retained.cleanup.iter().flatten().enumerate() {
            assert_eq!(context.error.evidence.as_ref().unwrap().errno, index as i32);
        }
        engine.handle_event(SessionEvent::CleanupBlocked {
            attempt: key.attempt,
            failure: first.unwrap(),
        });
        assert_eq!(engine.diagnostic_failures(), &retained);
        engine.close(engine.model.state_identity()).unwrap();
        engine.runner.barrier(key.attempt);
        engine.poll();
        assert_eq!(engine.diagnostic_failures().cleanup, retained.cleanup);
        assert_eq!(
            engine
                .diagnostic_failures()
                .candidate
                .as_ref()
                .unwrap()
                .as_ref(),
            candidate.as_ref()
        );
    }

    #[test]
    fn diagnostic_internal_watch_and_no_resource_paths_capture_before_synchronous_effects() {
        let mut engine = engine();
        engine.validator.reject_watch = Some(SubmitFailure::Disconnected);
        assert!(
            engine
                .apply(engine.model.state_identity(), engine.model.draft().revision)
                .is_err()
        );
        let validation = engine.diagnostic_failures().validation.as_ref().unwrap();
        assert_eq!(validation.identity.phase(), ProductPhase::Validating);
        assert_eq!(validation.error.operation, "watch_capture");

        let mut engine = reach_step(1);
        let old = engine.lease.as_ref().unwrap().key.attempt;
        engine.runner.start_failure = Some(StartFailure::NoResourcesCreated(failure(
            settings(30),
            FailureCategory::Lifecycle(LifecycleFailure::OwnerSpawn),
            "private spawn",
        )));
        engine.runner.barrier(old);
        engine.poll();
        let candidate = engine.diagnostic_failures().candidate.as_ref().unwrap();
        assert_eq!(candidate.identity.phase(), ProductPhase::OpeningCandidate);
        assert_eq!(engine.model.phase(), ProductPhase::ValidatingPrior);
    }

    #[test]
    fn diagnostic_late_fault_preserves_old_candidate_from_restored_active() {
        let mut engine = reach_step(5);
        engine.runner.verified();
        engine.poll();
        let original = engine.diagnostic_failures().candidate.clone().unwrap();
        let confirmed = engine.model.confirmed_treatment_key().unwrap();
        engine
            .runner
            .events
            .push_back(SessionEvent::TreatmentFault {
                key: confirmed,
                failure: treatment_error(
                    confirmed,
                    TreatmentErrorKind::Unconfirmed {
                        reason: TreatmentConfirmationFailure::BackendUnavailable,
                    },
                    true,
                ),
            });
        engine.poll();
        assert_eq!(
            engine
                .diagnostic_failures()
                .candidate
                .as_ref()
                .unwrap()
                .as_ref(),
            original.as_ref()
        );
        assert_eq!(
            engine
                .diagnostic_failures()
                .incumbent
                .as_ref()
                .unwrap()
                .identity
                .phase(),
            ProductPhase::ErrorWithActiveRestored,
        );
    }

    #[test]
    fn diagnostic_identical_open_errors_replace_new_keys_and_same_key_terminal_upgrades() {
        use crate::app::diagnostic::DiagnosticFailureCorrelation;
        let mut engine = engine();
        let error = failure(settings(60), FailureCategory::Session, "same");
        let mut previous = None;
        let mut stale = None;
        for _ in 0..2 {
            apply(&mut engine);
            validate(&mut engine);
            let key = engine.lease.as_ref().unwrap().key;
            if let Some(stale) = stale {
                engine.handle_event(SessionEvent::OpenFailed {
                    key: stale,
                    failure: error.clone(),
                });
                assert!(engine.diagnostic_failures().candidate.is_none());
            }
            engine.handle_event(SessionEvent::OpenFailed {
                key,
                failure: error.clone(),
            });
            let context = engine.diagnostic_failures().candidate.clone().unwrap();
            assert_eq!(
                context.correlation,
                DiagnosticFailureCorrelation::Attempt(key)
            );
            assert_ne!(previous, Some(context.correlation));
            assert_eq!(context.identity.phase(), ProductPhase::OpeningCandidate);
            engine.handle_event(SessionEvent::OpenFailed {
                key,
                failure: error.clone(),
            });
            assert_eq!(
                engine
                    .diagnostic_failures()
                    .candidate
                    .as_ref()
                    .unwrap()
                    .identity
                    .phase(),
                ProductPhase::CleaningFailedCandidate
            );
            previous = Some(context.correlation);
            stale = Some(key);
            engine.runner.barrier(key.attempt);
            engine.poll();
        }
    }

    #[test]
    fn diagnostic_successful_treatment_admission_and_confirmation_clear_failure_history() {
        let mut engine = reach_step(5);
        engine.runner.verified();
        engine.poll();
        assert!(engine.diagnostic_failures().candidate.is_some());
        let key = admit_treatments(&mut engine, renderer_settings(39));
        assert_eq!(
            engine.diagnostic_failures(),
            &DiagnosticFailureContexts::default()
        );
        treatment_success(&mut engine, key);
        engine.poll();
        assert_eq!(engine.model.phase(), ProductPhase::Active);
        assert_eq!(
            engine.diagnostic_failures(),
            &DiagnosticFailureContexts::default()
        );
    }

    #[test]
    fn diagnostic_recovery_unknown_inventory_is_keyed_once_and_refreshes_only_new_observations() {
        let mut engine = engine();
        let old = initial(&mut engine);
        let lost = observation(
            &engine,
            2,
            crate::domain::capture::VideoPresence::Absent,
            SourcePresence::Disabled,
            Some(2),
        );
        observe(&mut engine, lost);
        engine.runner.barrier(old);
        engine.poll();
        let unknown = observation(
            &engine,
            3,
            crate::domain::capture::VideoPresence::Unknown(failure(
                settings(60),
                FailureCategory::Validation(ValidationLayer::Discovery),
                "private scan",
            )),
            SourcePresence::Disabled,
            None,
        );
        observe(&mut engine, unknown.clone());
        let original = engine.diagnostic_failures().candidate.clone().unwrap();
        assert_eq!(original.identity.phase(), ProductPhase::Disconnected);
        engine.poll();
        observe(&mut engine, unknown);
        assert_eq!(
            engine
                .diagnostic_failures()
                .candidate
                .as_ref()
                .unwrap()
                .as_ref(),
            original.as_ref()
        );
        let next = observation(
            &engine,
            4,
            crate::domain::capture::VideoPresence::Unknown(failure(
                settings(60),
                FailureCategory::Validation(ValidationLayer::Discovery),
                "private scan",
            )),
            SourcePresence::Disabled,
            None,
        );
        observe(&mut engine, next);
        assert_ne!(
            engine
                .diagnostic_failures()
                .candidate
                .as_ref()
                .unwrap()
                .correlation,
            original.correlation
        );
        assert_eq!(
            engine
                .diagnostic_failures()
                .incumbent
                .as_ref()
                .unwrap()
                .identity
                .phase(),
            ProductPhase::Active
        );
    }

    #[test]
    fn diagnostic_unexpected_owner_stop_retains_primary_and_independent_cleanup_contexts() {
        let mut engine = engine();
        let attempt = initial(&mut engine);
        let identity = engine.model.state_identity();
        engine.handle_event(SessionEvent::OwnerStopped {
            attempt,
            outcome: Err(failure(
                settings(60),
                FailureCategory::Lifecycle(LifecycleFailure::Quiescence),
                "private owner",
            )),
        });
        let primary = engine.diagnostic_failures().incumbent.as_ref().unwrap();
        let cleanup = engine.diagnostic_failures().cleanup[0].as_ref().unwrap();
        assert_eq!(primary.identity, identity);
        assert_eq!(primary.error.operation, "owner_stopped");
        assert_eq!(
            primary.error.category,
            FailureCategory::Lifecycle(LifecycleFailure::Acknowledgement)
        );
        assert_eq!(cleanup.identity.phase(), ProductPhase::ErrorWithoutActive);
        assert_eq!(
            cleanup.error.category,
            FailureCategory::Lifecycle(LifecycleFailure::Quiescence)
        );
        let retained = engine.diagnostic_failures().clone();
        engine.handle_event(SessionEvent::OwnerStopped {
            attempt,
            outcome: Ok(()),
        });
        assert_eq!(engine.diagnostic_failures(), &retained);
        engine.handle_event(SessionEvent::NativeReleased { attempt });
        assert_eq!(engine.diagnostic_failures(), &retained);
    }
}
