//! Cold, bounded companions to the product model's retained failure slots.
//!
//! This module observes mutations; it never admits a command or drives an effect.
//! In particular, equal errors are not evidence that a scalar lane was unchanged.

use std::sync::Arc;

use crate::domain::{
    capture::{
        AudioEpoch, CaptureModeRequest, CapturedFourCc, ObservationEpoch, RecoveryObservation,
        VideoPresence, WatchId, WatchStamp,
    },
    failure::{
        ApplyFailure, BackendEvidence, Cause, FailureCategory, RendererErrorKind, Stage,
        TreatmentConfirmationFailure, TreatmentErrorKind, TreatmentFailureDisposition,
    },
    render::RendererProperty,
    state::{
        ApplyId, AttemptId, AttemptKey, AttemptPurpose, CleanupStatus, DraftRevision, ProductModel,
        ProductPhase, ProductState, StateIdentity, TreatmentAttemptKey, TreatmentPass,
        TreatmentRestoreRoute, ValidationKey, ValidationRequest,
    },
};

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DiagnosticFailureContexts {
    pub validation: Option<Arc<DiagnosticFailureContext>>,
    pub candidate: Option<Arc<DiagnosticFailureContext>>,
    pub restore: Option<Arc<DiagnosticFailureContext>>,
    pub resume: Option<Arc<DiagnosticFailureContext>>,
    pub incumbent: Option<Arc<DiagnosticFailureContext>>,
    pub cleanup: [Option<Arc<DiagnosticFailureContext>>; 6],
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DiagnosticFailureContext {
    pub identity: StateIdentity,
    pub correlation: DiagnosticFailureCorrelation,
    pub error: DiagnosticFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticFailureCorrelation {
    Validation {
        key: ValidationKey,
        revision: DraftRevision,
        watch: WatchStamp,
    },
    Attempt(AttemptKey),
    Treatment(TreatmentAttemptKey),
    Owner {
        attempt: AttemptId,
        audio_epoch: Option<AudioEpoch>,
    },
    Observation {
        stamp: WatchStamp,
        validation: Option<ValidationKey>,
        attempt: Option<AttemptId>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticCaptureTuple {
    pub vendor_id: u16,
    pub product_id: u16,
    pub mode: CaptureModeRequest,
    pub resolved_fourcc: Option<CapturedFourCc>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DiagnosticFailure {
    pub requested: DiagnosticCaptureTuple,
    pub category: FailureCategory,
    pub stage: Stage,
    pub cause: Cause,
    pub operation: &'static str,
    pub evidence: Option<BackendEvidence>,
    pub treatment: Option<DiagnosticTreatmentFailure>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DiagnosticTreatmentFailure {
    pub key: Option<TreatmentAttemptKey>,
    pub disposition: TreatmentFailureDisposition,
    pub kind: DiagnosticTreatmentError,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DiagnosticTreatmentError {
    Renderer {
        property: RendererProperty,
        kind: RendererErrorKind,
    },
    Unconfirmed {
        reason: TreatmentConfirmationFailure,
    },
}

impl From<&ApplyFailure> for DiagnosticFailure {
    fn from(error: &ApplyFailure) -> Self {
        let video = &error.requested.video;
        Self {
            requested: DiagnosticCaptureTuple {
                vendor_id: video.identity.vendor_id(),
                product_id: video.identity.product_id(),
                mode: video.mode,
                resolved_fourcc: video.resolved_fourcc,
            },
            category: error.category,
            stage: error.stage,
            cause: error.cause,
            operation: operation_code(&error.operation),
            evidence: error.evidence.clone(),
            treatment: error
                .treatment
                .as_ref()
                .map(|failure| DiagnosticTreatmentFailure {
                    key: failure.key,
                    disposition: failure.disposition,
                    kind: match &failure.kind {
                        TreatmentErrorKind::Renderer(renderer) => {
                            DiagnosticTreatmentError::Renderer {
                                property: renderer.property,
                                kind: renderer.kind.clone(),
                            }
                        }
                        TreatmentErrorKind::Unconfirmed { reason } => {
                            DiagnosticTreatmentError::Unconfirmed { reason: *reason }
                        }
                    },
                }),
        }
    }
}

// Never return the borrowed input: operations from adapters may include native
// prose or paths. Unknown operations have an explicit, non-personal code.
fn operation_code(operation: &str) -> &'static str {
    match operation {
        "watch_capture" => "watch_capture",
        "submit_validation" => "submit_validation",
        "validation_result" => "validation_result",
        "validation_removed" => "validation_removed",
        "opening_presence" => "opening_presence",
        "watch_cutover" => "watch_cutover",
        "committed_watch" => "committed_watch",
        "restore_watch" => "restore_watch",
        "treatment_restore_watch" => "treatment_restore_watch",
        "treatment_restore_capacity" => "treatment_restore_capacity",
        "video_removed" => "video_removed",
        "stream_ended" => "stream_ended",
        "begin_open" => "begin_open",
        "open_receipt" => "open_receipt",
        "owner_stopped" => "owner_stopped",
        "audio_detach" => "audio_detach",
        "stop" => "stop",
        "treatment" => "treatment",
        "renderer" => "renderer",
        "treatment_verification" => "treatment_verification",
        "owner_open" => "owner_open",
        "startup_readiness_timeout" => "startup_readiness_timeout",
        "owner_stop" => "owner_stop",
        "owner_spawn" => "owner_spawn",
        "owner_disconnect" => "owner_disconnect",
        "owner_ack_mismatch" => "owner_ack_mismatch",
        "surface_handoff" => "surface_handoff",
        "surface_lost" => "surface_lost",
        "surface_loss_barrier" => "surface_loss_barrier",
        "audio_cancel" => "audio_cancel",
        "validate_identity" => "validate_identity",
        "validate_mode" => "validate_mode",
        "validate_input" => "validate_input",
        "open" => "open",
        "negotiate" => "negotiate",
        _ => "Other",
    }
}

#[derive(Clone, Copy)]
pub(super) enum Mutation {
    Admission,
    TreatmentAdmission,
    Validation(DiagnosticFailureCorrelation, bool),
    Open(AttemptKey),
    Session(AttemptId, Option<AudioEpoch>),
    VideoLoss(AttemptId),
    Treatment(TreatmentAttemptKey),
    TreatmentRefused(TreatmentAttemptKey),
    Incumbent(TreatmentAttemptKey),
    LateTreatment(TreatmentAttemptKey),
    Cleanup(AttemptId),
    Barrier(AttemptId),
    Cutover(AttemptKey),
    Observation {
        stamp: WatchStamp,
        removal: Option<ObservationEpoch>,
        presence: Presence,
    },
    ContinueRecovery,
    Selection,
    Verified,
    Confirmed,
    ValidationSucceeded,
    Stop,
    Drain,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Presence {
    Present,
    Absent,
    Ambiguous,
    Unknown,
}

impl Mutation {
    pub(super) fn observation(observation: &RecoveryObservation) -> Self {
        Self::Observation {
            stamp: observation.stamp,
            removal: observation.last_video_removal,
            presence: match observation.video {
                VideoPresence::Present => Presence::Present,
                VideoPresence::Absent => Presence::Absent,
                VideoPresence::Ambiguous(_) => Presence::Ambiguous,
                VideoPresence::Unknown(_) => Presence::Unknown,
            },
        }
    }
    pub(super) fn validation(model: &ProductModel, request: &ValidationRequest) -> Self {
        Self::Validation(
            DiagnosticFailureCorrelation::Validation {
                key: request.key,
                revision: request.revision,
                watch: request.watch,
            },
            model.validation_request() == Some(request),
        )
    }
}

#[derive(Default)]
pub(super) struct FailureContextState {
    visible: DiagnosticFailureContexts,
    completed: DiagnosticFailureContexts,
    origin: Option<(ApplyId, DiagnosticFailureContexts)>,
    loss: Option<Arc<DiagnosticFailureContext>>,
    loss_from_treatment: bool,
    // The model intentionally exposes only Opening, not Cleaning. Retain the
    // exact already-accepted terminal key solely to correlate same-key upgrades.
    cleaning: Option<AttemptKey>,
    // Metadata companions to the watch high-water and recovery-attempt latch;
    // these never authorize a transition or suppress a model call.
    consumed_removal: Option<(WatchId, ObservationEpoch)>,
    recovery_attempted: Option<WatchStamp>,
    treatment_restore: bool,
}

#[derive(Clone, Copy)]
struct Before {
    identity: StateIdentity,
    correlation: Option<DiagnosticFailureCorrelation>,
    opening: Option<AttemptKey>,
    validation: Option<DiagnosticFailureCorrelation>,
    treatment: Option<TreatmentAttemptKey>,
    fresh: bool,
    active: Option<AttemptId>,
    transition: bool,
    restored: bool,
    error: bool,
    incumbent: bool,
    restore: bool,
    cleanup_len: usize,
    cleanup_complete: bool,
    recovery: bool,
    removal: bool,
    removal_epoch: Option<ObservationEpoch>,
    request_watch: Option<WatchStamp>,
    choice: bool,
    treatment_restore: bool,
    accepted: bool,
}

impl FailureContextState {
    pub(super) fn visible(&self) -> &DiagnosticFailureContexts {
        &self.visible
    }

    /// Capture before, invoke the existing model operation exactly once, install
    /// only its accepted writes, and return its original result unchanged.
    pub(super) fn record<T>(
        &mut self,
        model: &mut ProductModel,
        mutation: Mutation,
        input: Option<DiagnosticFailure>,
        call: impl FnOnce(&mut ProductModel) -> T,
    ) -> T {
        let before = self.before(model, mutation);
        let result = call(model);
        self.after(model, mutation, before, input);
        result
    }

    fn before(&self, model: &ProductModel, mutation: Mutation) -> Before {
        let identity = model.state_identity();
        let opening = model.opening().map(|(key, _)| key);
        let validation =
            model
                .validation_request()
                .map(|request| DiagnosticFailureCorrelation::Validation {
                    key: request.key,
                    revision: request.revision,
                    watch: request.watch,
                });
        let treatment = model.treating().map(|value| value.key());
        let fresh = model
            .treating()
            .is_some_and(|value| value.route() == Some(TreatmentRestoreRoute::FreshOwner));
        let active = model.active().map(|value| value.attempt());
        let correlation = match mutation {
            Mutation::Validation(key, _) => Some(key),
            Mutation::Open(key) | Mutation::Cutover(key) => {
                Some(DiagnosticFailureCorrelation::Attempt(key))
            }
            Mutation::Treatment(key)
            | Mutation::TreatmentRefused(key)
            | Mutation::Incumbent(key)
            | Mutation::LateTreatment(key) => Some(DiagnosticFailureCorrelation::Treatment(key)),
            Mutation::Session(attempt, audio_epoch) => Some(DiagnosticFailureCorrelation::Owner {
                attempt,
                audio_epoch,
            }),
            Mutation::Cleanup(attempt)
            | Mutation::Barrier(attempt)
            | Mutation::VideoLoss(attempt) => Some(DiagnosticFailureCorrelation::Owner {
                attempt,
                audio_epoch: None,
            }),
            Mutation::Observation { stamp, .. } => {
                Some(DiagnosticFailureCorrelation::Observation {
                    stamp,
                    validation: model.validation_request().map(|request| request.key),
                    attempt: identity.attempt(),
                })
            }
            Mutation::ContinueRecovery => {
                model
                    .observation()
                    .map(|observation| DiagnosticFailureCorrelation::Observation {
                        stamp: observation.stamp,
                        validation: None,
                        attempt: identity.attempt(),
                    })
            }
            _ => None,
        };
        let accepted = match mutation {
            Mutation::Validation(_, accepted) => accepted,
            Mutation::Open(key) => {
                opening == Some(key)
                    || (self.cleaning == Some(key) && identity.attempt() == Some(key.attempt))
            }
            Mutation::Treatment(key) => treatment == Some(key) && !fresh,
            Mutation::TreatmentRefused(key) => {
                treatment == Some(key) && (key.pass != TreatmentPass::Restore || !fresh)
            }
            Mutation::Incumbent(_) => treatment.is_some(),
            Mutation::Cleanup(attempt) | Mutation::Barrier(attempt) => {
                identity.attempt() == Some(attempt)
            }
            Mutation::Session(attempt, _) => {
                identity.attempt() == Some(attempt)
                    || self
                        .loss
                        .as_ref()
                        .is_some_and(|loss| match loss.correlation {
                            DiagnosticFailureCorrelation::Owner { attempt: lost, .. } => {
                                lost == attempt
                            }
                            _ => loss.identity.attempt() == Some(attempt),
                        })
            }
            Mutation::VideoLoss(attempt) => {
                identity.attempt() == Some(attempt)
                    && !matches!(
                        identity.phase(),
                        ProductPhase::Stopping | ProductPhase::ShutdownReady
                    )
            }
            Mutation::Observation { stamp, .. } => {
                model
                    .watch_target()
                    .is_some_and(|target| target.watch == stamp.watch)
                    && model
                        .observation()
                        .is_none_or(|old| old.stamp.epoch.get() < stamp.epoch.get())
                    && !matches!(
                        identity.phase(),
                        ProductPhase::Stopping | ProductPhase::ShutdownReady
                    )
            }
            _ => true,
        };
        let request = model
            .validation_request()
            .or_else(|| model.opening_request());
        let removal = match mutation {
            Mutation::Observation {
                stamp,
                removal: Some(epoch),
                ..
            } => {
                epoch.get() <= stamp.epoch.get()
                    && self
                        .consumed_removal
                        .is_none_or(|(watch, old)| watch != stamp.watch || epoch.get() > old.get())
            }
            _ => false,
        };
        Before {
            identity,
            correlation,
            opening,
            validation,
            treatment,
            fresh,
            active,
            transition: matches!(
                model.state(),
                ProductState::Applying(_) | ProductState::Recovering(_)
            ),
            restored: matches!(model.state(), ProductState::ErrorWithActiveRestored { .. }),
            error: matches!(model.state(), ProductState::ErrorWithoutActive { .. }),
            incumbent: model
                .failures()
                .is_some_and(|report| report.incumbent.is_some()),
            restore: model
                .failures()
                .is_some_and(|report| report.restore.is_some()),
            cleanup_len: model.failures().map_or(0, |report| report.cleanup.len()),
            cleanup_complete: matches!(model.cleanup(), CleanupStatus::Complete),
            recovery: model.recovery().is_some(),
            accepted,
            removal_epoch: match mutation {
                Mutation::Observation { removal, .. } => removal,
                _ => None,
            },
            removal,
            request_watch: request.map(|request| request.watch),
            choice: request.is_some_and(|request| request.choice.is_some()),
            treatment_restore: self.treatment_restore,
        }
    }

    fn context(before: &Before, error: DiagnosticFailure) -> Arc<DiagnosticFailureContext> {
        Arc::new(DiagnosticFailureContext {
            identity: before.identity,
            correlation: before
                .correlation
                .expect("failure mutation has a concrete correlation"),
            error,
        })
    }

    fn assign(
        &mut self,
        model: &ProductModel,
        before: &Before,
        lane: Lane,
        input: Option<&DiagnosticFailure>,
    ) {
        if let Some(error) = lane.error(model) {
            // Direct calls freeze their input before mutation. Internally
            // generated writes instead project the model's retained value.
            let projected = input
                .cloned()
                .unwrap_or_else(|| DiagnosticFailure::from(error));
            *lane.slot(&mut self.visible) = Some(Self::context(before, projected));
        }
    }

    fn validate(
        &mut self,
        model: &ProductModel,
        before: &Before,
        input: Option<&DiagnosticFailure>,
    ) {
        let Some(rejection) = model.validation_rejection() else {
            return;
        };
        self.visible.validation = Some(Self::context(
            before,
            input
                .cloned()
                .unwrap_or_else(|| DiagnosticFailure::from(&rejection.failure)),
        ));
        let purpose = match before.validation {
            Some(DiagnosticFailureCorrelation::Validation { key, .. }) => key.purpose,
            _ => return,
        };
        if purpose == AttemptPurpose::Restore {
            self.assign(model, before, Lane::Restore, input);
        } else if matches!(
            purpose,
            AttemptPurpose::Recovery | AttemptPurpose::Reconnect
        ) || before.recovery
        {
            self.assign(model, before, Lane::Candidate, input);
        } else if matches!(model.state(), ProductState::ErrorWithActiveRestored { .. })
            || (before.error
                || (before.active.is_none()
                    && !before.incumbent
                    && before.identity.attempt().is_none()
                    && before.cleanup_complete))
                && matches!(model.state(), ProductState::ErrorWithoutActive { .. })
        {
            if let Some((_, origin)) = &self.origin {
                let validation = self.visible.validation.take();
                self.visible = origin.clone();
                self.visible.validation = validation;
            }
        } else if matches!(
            model.state(),
            ProductState::Active(_) | ProductState::Stopped
        ) {
            let validation = self.visible.validation.take();
            self.visible = self.completed.clone();
            self.visible.validation = validation;
        }
    }

    fn after(
        &mut self,
        model: &ProductModel,
        mutation: Mutation,
        before: Before,
        input: Option<DiagnosticFailure>,
    ) {
        let input = input.as_ref();
        if before.accepted {
            match mutation {
                Mutation::Admission => {
                    if matches!(
                        model.state(),
                        ProductState::Applying(_) | ProductState::Recovering(_)
                    ) && !before.transition
                    {
                        let admission = model
                            .state_identity()
                            .operation()
                            .expect("admitted operation");
                        self.origin = (before.restored || before.error)
                            .then(|| (admission, self.visible.lanes()));
                        self.visible = if self.loss_from_treatment
                            && model.validation_request().is_some_and(|request| {
                                request.key.purpose == AttemptPurpose::Recovery
                            }) {
                            self.completed.clone()
                        } else {
                            DiagnosticFailureContexts {
                                incumbent: self.loss.clone(),
                                ..Default::default()
                            }
                        };
                    }
                }
                Mutation::TreatmentAdmission => {
                    if model
                        .treating()
                        .is_some_and(|transition| Some(transition.key()) != before.treatment)
                    {
                        self.visible = DiagnosticFailureContexts::default();
                        self.completed = DiagnosticFailureContexts::default();
                        self.origin = None;
                    }
                }
                Mutation::Validation(_, _) => self.validate(model, &before, input),
                Mutation::Open(key) => {
                    self.assign(model, &before, Lane::for_purpose(key.purpose), input);
                    self.cleaning = Some(key);
                }
                Mutation::Treatment(key) | Mutation::TreatmentRefused(key) => self.assign(
                    model,
                    &before,
                    if key.pass == TreatmentPass::Restore {
                        Lane::Restore
                    } else {
                        Lane::Candidate
                    },
                    input,
                ),
                Mutation::Incumbent(_) => self.assign(model, &before, Lane::Incumbent, input),
                Mutation::Session(attempt, _) | Mutation::VideoLoss(attempt) => {
                    if before.recovery
                        && self
                            .loss
                            .as_ref()
                            .is_some_and(|loss| loss.identity.attempt() == Some(attempt))
                    {
                        self.enrich_loss(model, before.transition, !before.transition);
                    } else if let Some(key) = before.opening {
                        let opening = Before {
                            correlation: Some(DiagnosticFailureCorrelation::Attempt(key)),
                            ..before
                        };
                        self.assign(model, &opening, Lane::for_purpose(key.purpose), input);
                        self.cleaning = Some(key);
                    } else if let Some(key) = before.treatment {
                        self.assign(model, &before, Lane::Incumbent, input);
                        if !before.fresh {
                            let treatment = Before {
                                correlation: Some(DiagnosticFailureCorrelation::Treatment(key)),
                                ..before
                            };
                            self.assign(
                                model,
                                &treatment,
                                if key.pass == TreatmentPass::Restore {
                                    Lane::Restore
                                } else {
                                    Lane::Candidate
                                },
                                input,
                            );
                        }
                    } else if before.active == Some(attempt) {
                        self.assign(model, &before, Lane::Incumbent, input);
                    }
                }
                Mutation::LateTreatment(key) => {
                    if before.recovery
                        && self
                            .loss
                            .as_ref()
                            .is_some_and(|loss| loss.identity.attempt() == Some(key.attempt))
                    {
                        let visible = before.transition || before.restored || before.error;
                        self.enrich_loss(model, visible, !visible);
                    } else if model.state_identity().attempt() == Some(key.attempt)
                        && (before.active == Some(key.attempt)
                            || (before.transition
                                && matches!(
                                    before.identity.phase(),
                                    ProductPhase::ClosingOld | ProductPhase::ClosingResume
                                )))
                        && (model.treating().is_some_and(|transition| {
                            transition.route() == Some(TreatmentRestoreRoute::FreshOwner)
                        }) || matches!(model.state(), ProductState::ErrorWithoutActive { .. }))
                        && !matches!(model.cleanup(), CleanupStatus::Blocked { .. })
                    {
                        let capacity =
                            matches!(model.state(), ProductState::ErrorWithoutActive { .. });
                        if !before.restored || capacity {
                            self.visible = DiagnosticFailureContexts::default();
                            self.assign(model, &before, Lane::Candidate, input);
                        }
                        self.assign(model, &before, Lane::Incumbent, input);
                        if capacity {
                            self.assign(model, &before, Lane::Restore, None);
                        }
                    }
                }
                Mutation::Cleanup(_) => {
                    if let Some(report) = model.failures() {
                        for index in before.cleanup_len..report.cleanup.len().min(6) {
                            self.visible.cleanup[index] = Some(Self::context(
                                &before,
                                DiagnosticFailure::from(&report.cleanup[index]),
                            ));
                        }
                    }
                }
                Mutation::Barrier(_) => {
                    if !before.restore
                        && model
                            .failures()
                            .is_some_and(|report| report.restore.is_some())
                        && (before.transition || before.treatment.is_some() && before.fresh)
                    {
                        self.assign(model, &before, Lane::Restore, None);
                    }
                }
                Mutation::Cutover(_) => {
                    if before.opening.is_some()
                        && matches!(model.state(), ProductState::ErrorWithoutActive { .. })
                    {
                        self.assign(model, &before, Lane::Candidate, None);
                    }
                }
                Mutation::Observation {
                    stamp,
                    removal,
                    presence,
                } => {
                    if before.removal {
                        self.consumed_removal = removal.map(|epoch| (stamp.watch, epoch));
                    }
                    self.observation(model, &before, presence);
                }
                Mutation::ContinueRecovery => self.continued(model, &before),
                Mutation::Selection => {
                    self.completed = self.visible.lanes();
                }
                Mutation::Verified => {
                    if model.opening().is_none()
                        && model.active().is_some()
                        && !matches!(model.state(), ProductState::ErrorWithActiveRestored { .. })
                    {
                        self.completed = self.visible.lanes();
                    }
                }
                Mutation::Confirmed => {
                    if before.treatment.is_some()
                        && model.treating().is_none()
                        && matches!(model.state(), ProductState::Active(_))
                    {
                        self.visible = self.completed.clone();
                    }
                }
                Mutation::Stop => {
                    if matches!(model.state(), ProductState::Stopping { .. })
                        && model.failures().is_some()
                        && (before.transition
                            || before.treatment.is_some()
                            || before.restored
                            || before.error)
                    {
                        // stop only overwrites completed history for a nonempty report.
                        if !self.visible.is_empty() {
                            self.completed = self.visible.lanes();
                        }
                        self.visible = self.completed.clone();
                    }
                }
                Mutation::Drain | Mutation::ValidationSucceeded => {}
            }
        }
        self.synchronize(model, &before, mutation);
    }

    fn enrich_loss(&mut self, model: &ProductModel, visible: bool, completed: bool) {
        if let (Some(original), Some(loss)) = (&self.loss, model.recovery()) {
            let error = DiagnosticFailure::from(&loss.failure);
            let context = if original.error == error {
                original.clone()
            } else {
                Arc::new(DiagnosticFailureContext {
                    error,
                    ..original.as_ref().clone()
                })
            };
            self.loss = Some(context.clone());
            if completed {
                self.completed.incumbent = Some(context.clone());
                if matches!(
                    model.state(),
                    ProductState::Disconnected
                        | ProductState::SelectionRequired
                        | ProductState::Active(_)
                        | ProductState::Stopped
                        | ProductState::Stopping { .. }
                        | ProductState::ShutdownReady
                ) {
                    self.visible.incumbent = Some(context.clone());
                }
            }
            if visible
                && let Some(error) = Lane::Incumbent.error(model)
                && let Some(original) = &self.visible.incumbent
            {
                let error = DiagnosticFailure::from(error);
                if original.error != error {
                    self.visible.incumbent = Some(Arc::new(DiagnosticFailureContext {
                        error,
                        ..original.as_ref().clone()
                    }));
                }
            }
        }
    }

    fn observation(&mut self, model: &ProductModel, before: &Before, presence: Presence) {
        if before.removal && before.active.is_some() {
            // video_lost returns immediately, including duplicate loss evidence.
            if !before.recovery {
                self.assign(model, before, Lane::Incumbent, None);
            }
            return;
        }
        let removed_request = before.removal
            && before
                .request_watch
                .is_some_and(|watch| match before.correlation {
                    Some(DiagnosticFailureCorrelation::Observation { stamp, .. }) => {
                        watch.watch == stamp.watch
                            && before
                                .removal_epoch
                                .is_some_and(|epoch| epoch.get() >= watch.epoch.get())
                    }
                    _ => false,
                });
        if removed_request && before.transition {
            if before.treatment_restore {
                self.assign(model, before, Lane::Incumbent, None);
                self.completed = self.visible.lanes();
                self.loss_from_treatment = true;
                return;
            }
            if before.opening.is_some() || before.recovery {
                self.completed = self.visible.lanes();
                if before.opening.is_some() {
                    return;
                }
            } else if before.validation.is_some() {
                self.assign(model, before, Lane::Candidate, None);
            }
        } else if let Some(key) = before.opening {
            if presence != Presence::Present && !(before.choice && presence == Presence::Ambiguous)
            {
                let opening = Before {
                    correlation: Some(DiagnosticFailureCorrelation::Attempt(key)),
                    ..*before
                };
                self.assign(model, &opening, Lane::for_purpose(key.purpose), None);
                self.cleaning = Some(key);
                if matches!(
                    key.purpose,
                    AttemptPurpose::Recovery | AttemptPurpose::Reconnect
                ) {
                    self.recovery_attempted =
                        model.observation().map(|observation| observation.stamp);
                }
            }
            return;
        } else if let Some(DiagnosticFailureCorrelation::Validation { key, .. }) = before.validation
            && matches!(
                key.purpose,
                AttemptPurpose::Recovery | AttemptPurpose::Reconnect
            )
        {
            match presence {
                Presence::Unknown => {
                    let validation = Before {
                        correlation: before.validation,
                        ..*before
                    };
                    self.validate(model, &validation, None);
                    self.recovery_attempted =
                        model.observation().map(|observation| observation.stamp);
                    return;
                }
                Presence::Absent | Presence::Ambiguous
                    if !before.choice || presence == Presence::Absent =>
                {
                    self.completed = self.visible.lanes();
                }
                _ => {}
            }
        }
        self.continued(model, before);
    }

    fn continued(&mut self, model: &ProductModel, before: &Before) {
        if !matches!(model.state(), ProductState::ErrorWithoutActive { .. })
            || model.recovery().is_none()
            || model.state_identity().attempt().is_some()
            || !matches!(model.cleanup(), CleanupStatus::Complete)
        {
            return;
        }
        if let Some(observation) = model.observation()
            && matches!(observation.video, VideoPresence::Unknown(_))
            && self.recovery_attempted != Some(observation.stamp)
        {
            self.visible = self.completed.clone();
            self.assign(model, before, Lane::Candidate, None);
            self.recovery_attempted = Some(observation.stamp);
        }
    }

    fn synchronize(&mut self, model: &ProductModel, before: &Before, mutation: Mutation) {
        let transition = matches!(
            model.state(),
            ProductState::Applying(_) | ProductState::Recovering(_)
        );
        // Model-retained completed history is distinct from its visible report.
        if matches!(
            model.state(),
            ProductState::Disconnected | ProductState::SelectionRequired
        ) && (before.transition || before.active.is_some() || before.treatment.is_some())
        {
            self.completed = self.visible.lanes();
        }
        if matches!(mutation, Mutation::Cleanup(_))
            && matches!(
                before.identity.phase(),
                ProductPhase::Disconnected
                    | ProductPhase::SelectionRequired
                    | ProductPhase::Stopping
            )
        {
            self.completed = self.visible.lanes();
        }
        if let Some(rejection) = model.validation_rejection()
            && matches!(mutation, Mutation::Validation(_, _))
            && before.accepted
            && (matches!(
                rejection.request.key.purpose,
                AttemptPurpose::Recovery | AttemptPurpose::Reconnect
            ) || before.recovery)
        {
            self.recovery_attempted = model
                .observation()
                .map(|observation| observation.stamp)
                .or(Some(rejection.request.watch));
        }
        if matches!(mutation, Mutation::Open(key) if matches!(key.purpose, AttemptPurpose::Recovery | AttemptPurpose::Reconnect))
            && before.accepted
        {
            self.recovery_attempted = model
                .observation()
                .map(|observation| observation.stamp)
                .or(before.request_watch);
        }
        if model.validation_request().is_some_and(|request| {
            matches!(
                request.key.purpose,
                AttemptPurpose::Recovery | AttemptPurpose::Reconnect
            )
        }) && !before.transition
        {
            self.recovery_attempted = model.validation_request().map(|request| request.watch);
        }
        if self.consumed_removal.is_some_and(|(watch, _)| {
            model
                .watch_target()
                .is_none_or(|target| target.watch != watch)
        }) {
            self.consumed_removal = None;
        }
        if model.recovery().is_some() && !before.recovery {
            self.loss = self.visible.incumbent.clone();
            self.loss_from_treatment = before.treatment.is_some() || before.treatment_restore;
        } else if model.recovery().is_none() {
            self.loss = None;
            self.loss_from_treatment = false;
            self.recovery_attempted = None;
        }
        if transition && !before.transition && !matches!(mutation, Mutation::Admission) {
            let inherited = self.visible.lanes();
            let admission = model.state_identity().operation();
            self.origin = if before.error || before.restored {
                admission.map(|id| (id, inherited.clone()))
            } else {
                None
            };
            if model
                .validation_request()
                .is_some_and(|request| request.key.purpose != AttemptPurpose::Restore)
            {
                self.visible = if self.loss_from_treatment {
                    self.completed.clone()
                } else {
                    DiagnosticFailureContexts {
                        incumbent: self.loss.clone(),
                        ..Default::default()
                    }
                };
            }
        }
        if !transition || (matches!(mutation, Mutation::Admission) && !before.transition) {
            self.treatment_restore = false;
        } else if matches!(mutation, Mutation::Barrier(_))
            && before.treatment.is_some()
            && before.fresh
        {
            self.treatment_restore = true;
        }
        if matches!(mutation, Mutation::Drain)
            && before.identity.phase() == ProductPhase::Stopping
            && matches!(model.state(), ProductState::ErrorWithoutActive { .. })
        {
            self.completed = DiagnosticFailureContexts::default();
        }
        if !transition
            || self.origin.as_ref().is_some_and(|(admission, _)| {
                Some(*admission) != model.state_identity().operation()
            })
        {
            self.origin = None;
        }
        if !transition {
            self.cleaning = None;
        }
        if (before.transition || before.treatment.is_some() || before.restored || before.error)
            && matches!(
                model.state(),
                ProductState::Active(_)
                    | ProductState::Stopped
                    | ProductState::Disconnected
                    | ProductState::SelectionRequired
                    | ProductState::Stopping { .. }
                    | ProductState::ShutdownReady
            )
        {
            let validation = self.visible.validation.take();
            self.visible = self.completed.clone();
            self.visible.validation = validation;
        }
        self.visible.retain(model);
        if model.validation_rejection().is_none() {
            self.visible.validation = None;
        }
    }
}

#[derive(Clone, Copy)]
enum Lane {
    Candidate,
    Restore,
    Resume,
    Incumbent,
}
impl Lane {
    fn for_purpose(purpose: AttemptPurpose) -> Self {
        match purpose {
            AttemptPurpose::Restore => Self::Restore,
            AttemptPurpose::Resume => Self::Resume,
            _ => Self::Candidate,
        }
    }
    fn get(self, report: &crate::domain::state::FailureReport) -> Option<&ApplyFailure> {
        match self {
            Self::Candidate => report.candidate.as_ref(),
            Self::Restore => report.restore.as_ref(),
            Self::Resume => report.resume.as_ref(),
            Self::Incumbent => report.incumbent.as_ref(),
        }
    }
    fn error(self, model: &ProductModel) -> Option<&ApplyFailure> {
        self.get(model.failures()?)
    }
    fn slot(
        self,
        contexts: &mut DiagnosticFailureContexts,
    ) -> &mut Option<Arc<DiagnosticFailureContext>> {
        match self {
            Self::Candidate => &mut contexts.candidate,
            Self::Restore => &mut contexts.restore,
            Self::Resume => &mut contexts.resume,
            Self::Incumbent => &mut contexts.incumbent,
        }
    }
}
impl DiagnosticFailureContexts {
    fn lanes(&self) -> Self {
        Self {
            validation: None,
            ..self.clone()
        }
    }
    fn is_empty(&self) -> bool {
        self.candidate.is_none()
            && self.restore.is_none()
            && self.resume.is_none()
            && self.incumbent.is_none()
            && self.cleanup.iter().all(Option::is_none)
    }
    fn retain(&mut self, model: &ProductModel) {
        for lane in [
            Lane::Candidate,
            Lane::Restore,
            Lane::Resume,
            Lane::Incumbent,
        ] {
            if lane.error(model).is_none() {
                *lane.slot(self) = None;
            }
        }
        let len = model.failures().map_or(0, |report| report.cleanup.len());
        for slot in self.cleanup.iter_mut().skip(len) {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod operation_tests {
    use super::operation_code;

    #[test]
    fn startup_timeout_is_finite_and_unknown_adapter_operations_remain_private() {
        assert_eq!(
            operation_code("startup_readiness_timeout"),
            "startup_readiness_timeout"
        );
        for unknown in [
            "startup_readiness_timeout /dev/video0",
            "startup_readiness_timeout\nprivate native prose",
            "startup_readiness_timeout_suffix",
            "private adapter operation",
            "/home/private/capture",
        ] {
            assert_eq!(operation_code(unknown), "Other");
        }
    }
}
