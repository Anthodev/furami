//! Concrete one-owner SessionRunner. Native gate never chooses replacement policy.

use super::{
    controller::{
        FilterSnapshot, FilterStatus, Generation, MediaError, OwnerEndpoint, PlaybackIntent,
        SessionConfig, Snapshot, SurfaceToken,
    },
    filter_catalog::{CompiledFilterChain, FilterCapabilities, FilterCatalogError, compile_chain},
    session::{SessionFacts, VerificationStatus},
};
use crate::{
    app::{
        gate::{GatePhase, GateState, GateUpdate},
        ports::{
            AudioOutcome, FactStatus, FilterOpenReceipt, ImmediateIntent, OpenReadiness,
            OpenReceipt, SessionEvent, SessionRunner, StartFailure, StopReason, StopSubmission,
            SubmitStatus, VerificationSummary,
        },
    },
    capture::PreparedCapture,
    domain::{
        capture::{
            AudioAvailability, AudioEpoch, AudioSelection, AudioSilence, PlaybackGain, WatchStamp,
        },
        failure::{
            ApplyFailure, Cause, FailureCategory, FilterAttemptDiagnostics, FilterEntryMetadata,
            FilterErrorKind, FilterFailure, LifecycleFailure, Stage, ValidationLayer,
        },
        output::OutputPlan,
        state::{
            AttemptId, AttemptKey, DraftSettings, FilterAttemptKey, FilterPass, InitialPlayback,
            PauseRequestId,
        },
    },
};

type Spawner = dyn FnMut(Generation, SessionConfig) -> Result<OwnerEndpoint, MediaError>;

pub(crate) struct GateRunner {
    state: GateState,
    endpoint: Option<OwnerEndpoint>,
    spawn: Box<Spawner>,
    capabilities: Result<FilterCapabilities, FilterCatalogError>,
    key: Option<AttemptKey>,
    requested: Option<DraftSettings>,
    opening_result_sent: bool,
    verified: bool,
    failure_sent: bool,
    ended_sent: bool,
    failure_event: Option<SessionEvent>,
    owner_event: Option<SessionEvent>,
    release_event: Option<SessionEvent>,
    blocked_event: Option<SessionEvent>,
    secondary_blocked_event: Option<SessionEvent>,
    audio_event: Option<SessionEvent>,
    stream_event: Option<SessionEvent>,
    detached_event: Option<SessionEvent>,
    last_detached: Option<AudioEpoch>,
    audio_epoch: Option<AudioEpoch>,
    attach_admitted: Option<AudioEpoch>,
    epoch_event: Option<SessionEvent>,
    watch: Option<WatchStamp>,
    retained_progress: Option<Snapshot>,
    last_filter: Option<FilterSnapshot>,
    filter_event: Option<SessionEvent>,
    pause_request: Option<(PauseRequestId, bool)>,
    last_pause_request: Option<PauseRequestId>,
    create_native: bool,
    release_native: bool,
    dirty: bool,
    last_blocked: Option<ApplyFailure>,
    fatal_native: Option<ApplyFailure>,
    ended: bool,
    audio: AudioAvailability,
    pub(crate) report: String,
}

impl GateRunner {
    pub(crate) fn new(
        prefix: String,
        capabilities: Result<FilterCapabilities, FilterCatalogError>,
    ) -> Self {
        Self::with_spawner(capabilities, move |generation, config| {
            OwnerEndpoint::spawn(generation, prefix.clone(), config)
        })
    }
    pub(crate) fn with_spawner(
        capabilities: Result<FilterCapabilities, FilterCatalogError>,
        spawn: impl FnMut(Generation, SessionConfig) -> Result<OwnerEndpoint, MediaError> + 'static,
    ) -> Self {
        Self {
            state: GateState::new(),
            endpoint: None,
            spawn: Box::new(spawn),
            capabilities,
            key: None,
            requested: None,
            opening_result_sent: false,
            verified: false,
            failure_sent: false,
            ended_sent: false,
            failure_event: None,
            owner_event: None,
            release_event: None,
            blocked_event: None,
            secondary_blocked_event: None,
            audio_event: None,
            stream_event: None,
            detached_event: None,
            last_detached: None,
            audio_epoch: None,
            attach_admitted: None,
            epoch_event: None,
            watch: None,
            retained_progress: None,
            last_filter: None,
            filter_event: None,
            pause_request: None,
            last_pause_request: None,
            create_native: false,
            release_native: false,
            dirty: true,
            last_blocked: None,
            fatal_native: None,
            ended: false,
            audio: AudioAvailability::Disabled,
            report: String::new(),
        }
    }
    #[cfg(test)]
    pub(crate) fn phase(&self) -> GatePhase {
        self.state.phase()
    }
    #[cfg(test)]
    pub(crate) fn attempt(&self) -> Option<AttemptId> {
        self.state.attempt()
    }
    pub(crate) fn take_native_update(&mut self) -> GateUpdate {
        let mut update = self.state.unchanged();
        update.changed = std::mem::take(&mut self.dirty);
        update.create_native = std::mem::take(&mut self.create_native);
        update.release_native = std::mem::take(&mut self.release_native);
        update
    }
    fn apply_native(&mut self, update: GateUpdate) {
        self.dirty |= update.changed;
        self.create_native |= update.create_native;
        self.release_native |= update.release_native;
    }
    pub(crate) fn requested_audio(&self) -> Option<&AudioSelection> {
        self.requested.as_ref().map(|settings| &settings.audio)
    }
    pub(crate) fn fatal_native_failure(&self) -> Option<&ApplyFailure> {
        self.fatal_native.as_ref()
    }
    /// Borrow the already retained owner observation for cold structured output.
    /// Reading it never consumes the transaction event or builds diagnostics.
    pub(crate) fn filter_snapshot(&self) -> Option<&FilterSnapshot> {
        self.last_filter.as_ref()
    }
    fn map_failure(&self, error: MediaError) -> Option<ApplyFailure> {
        let requested = self.requested.clone()?;
        let lifecycle = match error.code {
            "owner_spawn" | "spawn" => Some(LifecycleFailure::OwnerSpawn),
            "surface_handoff" => Some(LifecycleFailure::SurfaceHandoff),
            "surface_lost" => Some(LifecycleFailure::SurfaceLoss),
            "owner_disconnect" | "surface_loss_barrier" | "owner_ack_mismatch" => {
                Some(LifecycleFailure::Acknowledgement)
            }
            "audio_cancel" => Some(LifecycleFailure::Quiescence),
            _ => None,
        };
        let category = lifecycle
            .map(FailureCategory::Lifecycle)
            .unwrap_or(FailureCategory::Session);
        let (stage, cause, evidence) = error
            .session
            .as_ref()
            .map(|session| (session.stage, session.cause, session.evidence.clone()))
            .unwrap_or((Stage::Unknown, Cause::Generic, None));
        Some(
            ApplyFailure::new(
                category,
                stage,
                cause,
                requested,
                error.code,
                error.to_string(),
            )
            .with_evidence(evidence),
        )
    }
    fn emit_failure(&mut self, failure: ApplyFailure) {
        let Some(key) = self.key else {
            return;
        };
        if self.failure_sent {
            return;
        }
        self.failure_sent = true;
        self.pause_request = None;
        self.discard_filter_confirmation();
        self.retained_progress = None;
        self.failure_event = Some(if self.verified {
            SessionEvent::SessionFailed {
                attempt: key.attempt,
                failure: failure.clone(),
            }
        } else {
            self.opening_result_sent = true;
            SessionEvent::OpenFailed {
                key,
                failure: failure.clone(),
            }
        });
        let update = self.state.fail(key.attempt, failure);
        self.apply_native(update);
        if let (Some(endpoint), Some(generation)) =
            (&self.endpoint, Generation::new(key.attempt.get()))
        {
            endpoint.stop(generation, None);
        }
    }
    fn emit_blocked(&mut self, failure: ApplyFailure) {
        let Some(key) = self.key else {
            return;
        };
        if self.last_blocked.as_ref() == Some(&failure) {
            return;
        }
        self.last_blocked = Some(failure.clone());
        let event = SessionEvent::CleanupBlocked {
            attempt: key.attempt,
            failure,
        };
        // Only surface poison and unavailable ack can produce distinct blockers
        // for one lease. Keep both terminal evidence slots, never coalesce them.
        if self.blocked_event.is_none() {
            self.blocked_event = Some(event);
        } else {
            self.secondary_blocked_event = Some(event);
        }
        self.dirty = true;
    }
    pub(crate) fn surface_ready(&mut self, token: SurfaceToken) {
        let Some(attempt) = AttemptId::new(token.generation.get()) else {
            return;
        };
        let update = self.state.surface_ready(attempt);
        if !update.changed {
            return;
        }
        self.apply_native(update);
        if self
            .endpoint
            .as_ref()
            .map(|endpoint| endpoint.attach(token))
            != Some(super::controller::SubmitStatus::Accepted)
            && let Some(failure) = self.map_failure(MediaError::new(
                "surface_handoff",
                "owner rejected native surface handoff",
            ))
        {
            self.emit_failure(failure);
        }
    }
    pub(crate) fn surface_lost(&mut self, attempt: AttemptId) {
        if self.state.attempt() != Some(attempt) || self.state.cleanup() == GatePhase::Releasing {
            return;
        }
        let Some(failure) = self.map_failure(MediaError::new(
            "surface_lost",
            "native surface destroyed before owner completion; generation revoked",
        )) else {
            return;
        };
        if self.fatal_native.is_none() {
            self.fatal_native = Some(failure.clone());
        }
        let update = self.state.surface_lost(attempt, failure.clone());
        self.apply_native(update);
        if let (Some(endpoint), Some(generation)) = (&self.endpoint, Generation::new(attempt.get()))
        {
            endpoint.stop(
                generation,
                Some(MediaError::new("surface_lost", failure.diagnostic.clone())),
            );
        }
        self.emit_failure(failure.clone());
        self.emit_blocked(failure);
    }
    /// Only historical Qt pre-destruction barrier blocks; normal port calls do not.
    pub(crate) fn wait_for_owner_ack(&mut self, attempt: AttemptId) -> Result<(), MediaError> {
        if self.state.attempt() != Some(attempt)
            || !self.state.blocked()
            || self.state.cleanup() != GatePhase::Stopping
        {
            return Err(MediaError::new(
                "surface_loss_barrier",
                "surface-loss barrier requires active failed attempt and live owner",
            ));
        }
        let result = match self.endpoint.as_mut() {
            Some(endpoint) => endpoint.wait_for_ack(),
            None => Err(MediaError::new(
                "surface_loss_barrier",
                "surface-loss barrier has no live owner endpoint",
            )),
        };
        if let Err(error) = &result
            && let Some(failure) = self.map_failure(error.clone())
        {
            let update = self.state.block(attempt, failure.clone());
            self.apply_native(update);
            self.emit_blocked(failure);
        }
        result
    }
    pub(crate) fn native_released(&mut self, attempt: AttemptId) {
        let update = self.state.native_released(attempt);
        if !update.changed {
            return;
        }
        self.apply_native(update);
        self.release_event = Some(SessionEvent::NativeReleased { attempt });
        tracing::info!(apply_id = self.key.map(|key| key.apply.get()), attempt_id = attempt.get(), requested = ?self.requested, "apply_native_released");
    }
    fn discard_filter_confirmation(&mut self) {
        if matches!(
            self.filter_event,
            Some(SessionEvent::FilterResult { result: Ok(_), .. })
        ) {
            self.filter_event = None;
        }
    }
    fn take_filter_failure(&mut self) -> Option<SessionEvent> {
        self.filter_event.take_if(|event| {
            matches!(
                event,
                SessionEvent::FilterResult { result: Err(_), .. }
                    | SessionEvent::FilterFault { .. }
            )
        })
    }
    fn project_filter_failure(&mut self, snapshot: &Snapshot) {
        if snapshot
            .filters
            .as_ref()
            .is_some_and(|filters| matches!(filters.status, FilterStatus::Failed(_)))
        {
            self.project_filters(snapshot);
        }
    }
    fn project_filters(&mut self, snapshot: &Snapshot) {
        let Some(open) = self.key else { return };
        let Some(filters) = &snapshot.filters else {
            return;
        };
        if filters.key.attempt != open.attempt
            || self.last_filter.as_ref().is_some_and(|last| {
                last.sequence > filters.sequence
                    || (last.key == filters.key
                        && last.sequence == filters.sequence
                        && last.status == filters.status)
            })
        {
            return;
        }
        let was_confirmed = self.last_filter.as_ref().is_some_and(|last| {
            last.key == filters.key && matches!(last.status, FilterStatus::Confirmed(_))
        });
        self.last_filter = Some(filters.clone());
        self.dirty = true;
        self.filter_event = match &filters.status {
            FilterStatus::Pending => None,
            FilterStatus::Confirmed(confirmation) => Some(SessionEvent::FilterResult {
                key: filters.key,
                result: Ok(*confirmation),
            }),
            FilterStatus::Failed(failure) => {
                if filters.key.pass == FilterPass::Open && !self.verified {
                    self.emit_failure(filter_apply_failure(
                        self.requested.as_ref().expect("owned opening settings"),
                        (**failure).clone(),
                    ));
                }
                Some(
                    if was_confirmed || self.verified && filters.key.pass == FilterPass::Open {
                        SessionEvent::FilterFault {
                            key: filters.key,
                            failure: failure.clone(),
                        }
                    } else {
                        SessionEvent::FilterResult {
                            key: filters.key,
                            result: Err(failure.clone()),
                        }
                    },
                )
            }
        };
    }
    fn snapshot(&mut self, snapshot: Snapshot) -> Option<SessionEvent> {
        let key = self.key?;
        if snapshot.generation.get() != key.attempt.get() {
            return None;
        }
        // Terminal observations prohibit success, not retention of a settled
        // exact-key negative. Classify it before any ended/cleanup early return.
        self.project_filter_failure(&snapshot);
        if snapshot.stream_ended.is_some()
            || snapshot.failure.is_some()
            || matches!(
                self.state.cleanup(),
                GatePhase::Releasing | GatePhase::Stopping
            )
        {
            self.discard_filter_confirmation();
        }
        if matches!(
            self.state.cleanup(),
            GatePhase::Releasing | GatePhase::Stopping
        ) {
            return self
                .failure_event
                .take()
                .or_else(|| self.take_filter_failure());
        }
        if let Some((reason, error)) = snapshot.stream_ended
            && !self.ended_sent
        {
            self.ended_sent = true;
            self.ended = true;
            self.pause_request = None;
            self.stream_event = Some(SessionEvent::StreamEnded {
                attempt: key.attempt,
                reason,
                error,
            });
        }
        if let Some((epoch, outcome)) = &snapshot.audio_detached
            && self.last_detached != Some(*epoch)
        {
            self.last_detached = Some(*epoch);
            if self.attach_admitted == Some(*epoch) {
                self.attach_admitted = None;
            }
            self.detached_event = Some(SessionEvent::AudioDetached {
                attempt: key.attempt,
                epoch: *epoch,
                outcome: outcome.clone(),
            });
        }
        if snapshot.audio_epoch != self.audio_epoch {
            self.audio_epoch = snapshot.audio_epoch;
            if let Some(epoch) = snapshot.audio_epoch {
                self.epoch_event = Some(SessionEvent::AudioAvailability {
                    attempt: key.attempt,
                    status: AudioAvailability::Opening { epoch },
                });
            }
        }
        if let Some(error) = &snapshot.failure {
            if let Some(filters) = &snapshot.filters
                && filters.key.attempt == key.attempt
                && let FilterStatus::Failed(failure) = &filters.status
                && let Some(settings) = &self.requested
            {
                self.emit_failure(filter_apply_failure(settings, (**failure).clone()));
            } else if let Some(failure) = self.map_failure(error.clone()) {
                self.emit_failure(failure);
            }
            return self
                .failure_event
                .take()
                .or_else(|| self.take_filter_failure())
                .or_else(|| self.stream_event.take());
        }
        self.dirty |= self.ended != snapshot.stream_ended.is_some() || self.audio != snapshot.audio;
        self.ended = snapshot.stream_ended.is_some();
        if self.ended {
            self.pause_request = None;
        }
        if self.audio != snapshot.audio {
            self.audio_event = Some(SessionEvent::AudioAvailability {
                attempt: key.attempt,
                status: snapshot.audio.clone(),
            });
            self.audio = snapshot.audio.clone();
        }
        if let Some(session) = &snapshot.session {
            let report = session.summary();
            self.dirty |= self.report != report;
            self.report = report;
        }
        if self.ended {
            return self
                .take_filter_failure()
                .or_else(|| self.stream_event.take());
        }
        self.project_filters(&snapshot);
        if let Some(event) = self.failure_event.take() {
            return Some(event);
        }
        if let Some(event) = self.filter_event.take() {
            self.retained_progress = Some(snapshot);
            return Some(event);
        }
        if let Some(event) = self
            .epoch_event
            .take()
            .or_else(|| self.detached_event.take())
            .or_else(|| self.audio_event.take())
        {
            self.retained_progress = Some(snapshot);
            return Some(event);
        }
        let update = self.state.progress(
            key.attempt,
            snapshot.initialized,
            filter_open_ready(key, &snapshot),
        );
        self.apply_native(update);
        if !self.opening_result_sent && self.state.phase() == GatePhase::Ready {
            match verified_receipt(key, self.requested.as_ref()?, &snapshot) {
                Ok(Some(receipt)) => {
                    self.opening_result_sent = true;
                    self.verified = true;
                    return Some(SessionEvent::OpenVerified {
                        key,
                        receipt: Box::new(receipt),
                    });
                }
                Ok(None) => {}
                Err(failure) => {
                    self.emit_failure(failure);
                    return self.failure_event.take();
                }
            }
        }
        if let Some(observation) = snapshot.pause
            && let Some(request) = observation.request
            && self.verified
            && self.pause_request == Some((request, observation.paused))
            && self.last_pause_request != Some(request)
        {
            self.pause_request = None;
            self.last_pause_request = Some(request);
            return Some(SessionEvent::PauseObserved {
                attempt: key.attempt,
                request,
                paused: observation.paused,
            });
        }
        self.detached_event
            .take()
            .or_else(|| self.audio_event.take())
    }
}

impl SessionRunner for GateRunner {
    type Prepared = PreparedCapture;
    type PreparedFilters = CompiledFilterChain;
    fn prepare_filters(
        &mut self,
        settings: &DraftSettings,
    ) -> Result<Self::PreparedFilters, ApplyFailure> {
        let result = match &self.capabilities {
            Ok(capabilities) => compile_chain(&settings.filters, capabilities),
            Err(error) => return Err(catalog_failure(settings, error)),
        };
        result.map_err(|error| catalog_failure(settings, &error))
    }
    fn begin_open(
        &mut self,
        key: AttemptKey,
        prepared: PreparedCapture,
        filters: Self::PreparedFilters,
        gain: PlaybackGain,
        playback: InitialPlayback,
        output: OutputPlan,
    ) -> Result<(), StartFailure> {
        let requested = prepared.settings().clone();
        if self.endpoint.is_some() || self.key.is_some() || self.state.blocked() {
            return Err(StartFailure::ResourcesCreated(ApplyFailure::new(
                FailureCategory::Lifecycle(LifecycleFailure::Protocol),
                Stage::Unknown,
                Cause::Generic,
                requested,
                "owner_open",
                "previous physical lease not retired",
            )));
        }
        let Some(generation) = Generation::new(key.attempt.get()) else {
            return Err(StartFailure::NoResourcesCreated(ApplyFailure::new(
                FailureCategory::Lifecycle(LifecycleFailure::Protocol),
                Stage::Unknown,
                Cause::Generic,
                requested,
                "owner_open",
                "invalid physical attempt",
            )));
        };
        let update = self.state.begin_attempt(key.attempt);
        if !update.create_native {
            return Err(StartFailure::NoResourcesCreated(ApplyFailure::new(
                FailureCategory::Lifecycle(LifecycleFailure::Protocol),
                Stage::Unknown,
                Cause::Generic,
                requested,
                "owner_open",
                "native gate rejected fresh attempt",
            )));
        }
        let event = serde_json::json!({
            "event": "Opening", "apply_id": key.apply.get(), "attempt_id": key.attempt.get(),
            "purpose": key.purpose, "requested": requested, "gain": gain,
        });
        tracing::info!(event = %event, "apply_open_submit");
        let (_, video, _, _, watch, selected_route) = prepared.into_parts();
        let config = SessionConfig {
            video,
            audio: requested.audio.clone(),
            gain,
            playback,
            output,
            watch,
            selected_route,
            filters: requested.filters.clone(),
            compiled_filters: filters,
            filter_key: FilterAttemptKey {
                apply: key.apply,
                attempt: key.attempt,
                pass: FilterPass::Open,
            },
        };
        // Worker spawn precedes native creation. Failure here proves no owner/host.
        let endpoint = match (self.spawn)(generation, config) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.state.retire_uncreated(key.attempt);
                return Err(StartFailure::NoResourcesCreated(ApplyFailure::new(
                    FailureCategory::Lifecycle(LifecycleFailure::OwnerSpawn),
                    Stage::Unknown,
                    Cause::Generic,
                    requested,
                    error.code,
                    error.to_string(),
                )));
            }
        };
        self.key = Some(key);
        self.requested = Some(requested);
        self.endpoint = Some(endpoint);
        self.opening_result_sent = false;
        self.verified = false;
        self.failure_sent = false;
        self.ended_sent = false;
        self.stream_event = None;
        self.detached_event = None;
        self.last_detached = None;
        self.audio_epoch = None;
        self.attach_admitted = None;
        self.epoch_event = None;
        self.watch = Some(watch);
        self.retained_progress = None;
        self.last_filter = None;
        self.filter_event = None;
        self.last_blocked = None;
        self.pause_request = None;
        self.last_pause_request = None;
        self.ended = false;
        self.audio = if self
            .requested
            .as_ref()
            .is_some_and(|settings| settings.audio.enabled())
        {
            AudioAvailability::Silent {
                reason: if playback == InitialPlayback::Paused {
                    AudioSilence::Paused
                } else {
                    AudioSilence::PendingRoute
                },
            }
        } else {
            AudioAvailability::Disabled
        };
        self.report.clear();
        self.apply_native(update);
        Ok(())
    }
    fn stop(&mut self, attempt: AttemptId, _reason: StopReason) -> StopSubmission {
        if self.state.attempt() != Some(attempt) {
            if self.endpoint.is_none() && self.state.attempt().is_none() {
                return StopSubmission::NoResourcesCreated;
            }
            return self
                .map_failure(MediaError::new(
                    "owner_stop",
                    "stop attempt does not own physical lease",
                ))
                .map(StopSubmission::Blocked)
                .unwrap_or(StopSubmission::AlreadyStopping);
        }
        if self.state.blocked()
            && let Some(failure) = self.state.failure()
        {
            return StopSubmission::Blocked(failure.clone());
        }
        if matches!(
            self.state.cleanup(),
            GatePhase::Stopping | GatePhase::Releasing
        ) {
            return StopSubmission::AlreadyStopping;
        }
        self.pause_request = None;
        self.retained_progress = None;
        self.discard_filter_confirmation();
        let update = self.state.stop(attempt);
        self.apply_native(update);
        if let (Some(endpoint), Some(generation)) = (&self.endpoint, Generation::new(attempt.get()))
        {
            endpoint.stop(generation, None);
        }
        StopSubmission::Accepted
    }
    fn poll(&mut self) -> Option<SessionEvent> {
        let event = (|| {
            let snapshot = match self
                .endpoint
                .as_ref()
                .and_then(OwnerEndpoint::take_snapshot)
            {
                Some(snapshot) => {
                    self.retained_progress = None;
                    Some(snapshot)
                }
                None => self.retained_progress.take(),
            };
            // Genuine terminal acknowledgement is inspected before coalesced progress.
            let stopped = self.endpoint.as_mut().map(OwnerEndpoint::take_stopped);
            match stopped {
                Some(Ok(Some(stopped))) => {
                    let key = self.key?;
                    if stopped.generation.get() != key.attempt.get() {
                        let failure = self.map_failure(MediaError::new(
                            "owner_ack_mismatch",
                            format!("destruction acknowledgement generation {} does not match owned attempt {}", stopped.generation.get(), key.attempt.get()),
                        ))?;
                        let update = self.state.block(key.attempt, failure.clone());
                        self.apply_native(update);
                        self.emit_failure(failure.clone());
                        self.emit_blocked(failure);
                        return self
                            .failure_event
                            .take()
                            .or_else(|| self.blocked_event.take());
                    }
                    let outcome = match stopped.outcome {
                        Ok(()) => Ok(()),
                        Err(error) => Err(self.map_failure(error)?),
                    };
                    if let Some(snapshot) = &snapshot {
                        self.project_filter_failure(snapshot);
                    }
                    if let Err(failure) = &outcome {
                        self.emit_failure(failure.clone());
                    }
                    let update = self
                        .state
                        .owner_stopped(key.attempt, outcome.as_ref().err().cloned());
                    self.apply_native(update);
                    self.endpoint = None;
                    if let Some(snapshot) = &snapshot
                        && let Some((reason, error)) = snapshot.stream_ended
                        && !self.ended_sent
                    {
                        self.ended_sent = true;
                        self.stream_event = Some(SessionEvent::StreamEnded {
                            attempt: key.attempt,
                            reason,
                            error,
                        });
                    }
                    self.audio = AudioAvailability::Disabled;
                    self.audio_event = None;
                    self.pause_request = None;
                    self.discard_filter_confirmation();
                    self.retained_progress = None;
                    self.owner_event = Some(SessionEvent::OwnerStopped {
                        attempt: key.attempt,
                        outcome,
                    });
                    tracing::info!(
                        apply_id = key.apply.get(),
                        attempt_id = key.attempt.get(),
                        "apply_owner_stopped"
                    );
                }
                Some(Err(error)) => {
                    let key = self.key?;
                    if let Some(failure) = self.map_failure(error) {
                        let update = self.state.block(key.attempt, failure.clone());
                        self.apply_native(update);
                        self.emit_failure(failure.clone());
                        self.emit_blocked(failure);
                    }
                }
                _ => {}
            }
            if let Some(event) = self.take_filter_failure() {
                return Some(event);
            }
            if let Some(event) = self.stream_event.take() {
                return Some(event);
            }
            if let Some(event) = self.failure_event.take() {
                return Some(event);
            }
            if let Some(event) = self.owner_event.take() {
                return Some(event);
            }
            if let Some(event) = self.blocked_event.take() {
                return Some(event);
            }
            if let Some(event) = self.secondary_blocked_event.take() {
                return Some(event);
            }
            if let Some(event) = self.release_event.take() {
                return Some(event);
            }
            if let Some(snapshot) = snapshot {
                return self.snapshot(snapshot);
            }
            self.epoch_event
                .take()
                .or_else(|| self.detached_event.take())
                .or_else(|| self.audio_event.take())
                .or_else(|| self.filter_event.take())
        })();
        if let Some(event) = &event {
            log_session_event(self.key, self.requested.as_ref(), event);
        }
        if matches!(&event, Some(SessionEvent::NativeReleased { .. })) {
            self.key = None;
            self.requested = None;
            self.audio_event = None;
            self.stream_event = None;
            self.detached_event = None;
            self.pause_request = None;
            self.last_pause_request = None;
            self.last_filter = None;
            self.filter_event = None;
        }
        event
    }
    fn submit_immediate(&mut self, attempt: AttemptId, intent: ImmediateIntent) -> SubmitStatus {
        if self.state.attempt() != Some(attempt) {
            return SubmitStatus::StaleGeneration;
        }
        if self.state.failure().is_some()
            || matches!(
                self.state.cleanup(),
                GatePhase::Stopping | GatePhase::Releasing
            )
        {
            return SubmitStatus::Closing;
        }
        match &intent {
            ImmediateIntent::AttachAudio {
                epoch,
                source,
                stamp,
            } => {
                let exact_source = matches!(self.requested_audio(),
                    Some(AudioSelection::Enabled { source: desired }) if desired == source);
                if (self.audio_epoch == Some(*epoch) || self.attach_admitted == Some(*epoch))
                    && self.watch == Some(*stamp)
                    && exact_source
                    && !matches!(
                        self.audio,
                        AudioAvailability::Blocked { .. } | AudioAvailability::Detaching { .. }
                    )
                {
                    return SubmitStatus::Accepted;
                }
                if self.state.phase() != GatePhase::Ready
                    || self.ended
                    || !self.verified
                    || self.audio_epoch.is_some()
                    || self.attach_admitted.is_some()
                    || matches!(
                        self.audio,
                        AudioAvailability::Blocked { .. } | AudioAvailability::Detaching { .. }
                    )
                    || self
                        .last_detached
                        .is_some_and(|old| epoch.get() <= old.get())
                    || self
                        .watch
                        .is_none_or(|current| current.watch != stamp.watch)
                    || (matches!(
                        self.audio,
                        AudioAvailability::Silent {
                            reason: AudioSilence::WaitingForSource(_)
                        }
                    ) && self
                        .watch
                        .is_some_and(|current| current.epoch.get() >= stamp.epoch.get()))
                    || !exact_source
                {
                    return SubmitStatus::NotReady;
                }
            }
            ImmediateIntent::DetachAudio { epoch }
                if self.audio_epoch != Some(*epoch) && self.last_detached != Some(*epoch) =>
            {
                return SubmitStatus::NotReady;
            }
            _ => {}
        }
        match &intent {
            ImmediateIntent::SetPaused { .. }
                if self.state.phase() != GatePhase::Ready || !self.state.has_surface() =>
            {
                return SubmitStatus::NotReady;
            }
            ImmediateIntent::SetGain(_) | ImmediateIntent::SetOutput(_)
                if !matches!(
                    self.state.phase(),
                    GatePhase::WaitingSurface | GatePhase::Opening | GatePhase::Ready
                ) =>
            {
                return SubmitStatus::NotReady;
            }
            _ => {}
        }
        let Some(generation) = Generation::new(attempt.get()) else {
            return SubmitStatus::StaleGeneration;
        };
        let pause_intent = match &intent {
            ImmediateIntent::SetPaused { request, paused } => Some((*request, *paused)),
            _ => None,
        };
        let attach_stamp = match &intent {
            ImmediateIntent::AttachAudio { epoch, stamp, .. } => Some((*epoch, *stamp)),
            _ => None,
        };
        let playback_intent = match intent {
            ImmediateIntent::SetPaused { request, paused } => {
                PlaybackIntent::SetPaused { request, paused }
            }
            ImmediateIntent::SetGain(gain) => PlaybackIntent::SetGain(gain),
            ImmediateIntent::SetOutput(output) => PlaybackIntent::SetOutput(output),
            ImmediateIntent::DetachAudio { epoch } => PlaybackIntent::DetachAudio { epoch },
            ImmediateIntent::AttachAudio {
                epoch,
                source,
                stamp,
            } => PlaybackIntent::AttachAudio {
                epoch,
                source,
                stamp,
            },
        };
        let status = self
            .endpoint
            .as_ref()
            .map(|endpoint| endpoint.submit(generation, playback_intent))
            .unwrap_or(super::controller::SubmitStatus::Closing);
        let mapped = match status {
            super::controller::SubmitStatus::Accepted => SubmitStatus::Accepted,
            super::controller::SubmitStatus::StaleGeneration => SubmitStatus::StaleGeneration,
            super::controller::SubmitStatus::NotReady => SubmitStatus::NotReady,
            super::controller::SubmitStatus::Closing => SubmitStatus::Closing,
            super::controller::SubmitStatus::CapacityExceeded => SubmitStatus::CapacityExceeded,
        };
        if mapped == SubmitStatus::Accepted
            && let Some((epoch, stamp)) = attach_stamp
        {
            self.attach_admitted = Some(epoch);
            self.watch = Some(stamp);
        }
        if mapped == SubmitStatus::Accepted
            && let Some(pause) = pause_intent
        {
            self.pause_request = Some(pause);
        }
        if matches!(
            mapped,
            SubmitStatus::Closing | SubmitStatus::CapacityExceeded
        ) {
            let error = if mapped == SubmitStatus::CapacityExceeded {
                MediaError::new(
                    "command_overflow",
                    "64-command owner queue full; session stopped",
                )
            } else {
                MediaError::new(
                    "owner_unavailable",
                    "owner no longer accepts playback commands",
                )
            };
            if let Some(failure) = self.map_failure(error) {
                self.emit_failure(failure);
            }
        }
        mapped
    }
    fn submit_filters(
        &mut self,
        key: FilterAttemptKey,
        filters: Self::PreparedFilters,
    ) -> SubmitStatus {
        if self.key.is_none_or(|open| open.attempt != key.attempt) {
            return SubmitStatus::StaleGeneration;
        }
        if self.state.failure().is_some()
            || matches!(
                self.state.cleanup(),
                GatePhase::Stopping | GatePhase::Releasing
            )
        {
            return SubmitStatus::Closing;
        }
        if !self.verified || self.pause_request.is_some() {
            return SubmitStatus::NotReady;
        }
        let generation = Generation::new(key.attempt.get()).expect("nonzero attempt");
        match self.endpoint.as_ref().map(|endpoint| {
            endpoint.submit(
                generation,
                PlaybackIntent::ApplyFilters {
                    key,
                    compiled: filters,
                },
            )
        }) {
            Some(super::controller::SubmitStatus::Accepted) => SubmitStatus::Accepted,
            Some(super::controller::SubmitStatus::StaleGeneration) => SubmitStatus::StaleGeneration,
            Some(super::controller::SubmitStatus::NotReady) => SubmitStatus::NotReady,
            Some(super::controller::SubmitStatus::CapacityExceeded) => {
                SubmitStatus::CapacityExceeded
            }
            Some(super::controller::SubmitStatus::Closing) | None => SubmitStatus::Closing,
        }
    }
}

fn fact_status(status: VerificationStatus) -> FactStatus {
    match status {
        VerificationStatus::Unverified => FactStatus::Unverified,
        VerificationStatus::ObservedCompatible => FactStatus::ObservedCompatible,
        VerificationStatus::Approximate => FactStatus::Approximate,
        VerificationStatus::Configured => FactStatus::Configured,
    }
}
fn catalog_failure(settings: &DraftSettings, error: &FilterCatalogError) -> ApplyFailure {
    let missing = matches!(error, FilterCatalogError::MissingCapability { .. });
    let failure = FilterFailure {
        kind: if missing {
            FilterErrorKind::Prevalidation
        } else {
            FilterErrorKind::CatalogUnavailable
        },
        attributed_ordinal: match error {
            FilterCatalogError::MissingCapability { filter, .. } => settings
                .filters
                .entries()
                .iter()
                .position(|entry| entry.kind() == *filter),
            _ => None,
        },
        requires_fresh_owner: false,
        diagnostics: FilterAttemptDiagnostics {
            key: None,
            entries: settings
                .filters
                .entries()
                .iter()
                .enumerate()
                .map(|(ordinal, entry)| FilterEntryMetadata {
                    ordinal,
                    label: entry.label().to_owned(),
                    enabled: entry.enabled(),
                })
                .collect(),
            records: Vec::new(),
            native_evidence_lost: false,
            truncated: false,
            dropped_context: 0,
        },
    };
    ApplyFailure::new(
        FailureCategory::Validation(ValidationLayer::Filters),
        Stage::Prevalidation,
        Cause::Generic,
        settings.clone(),
        "filter_prepare",
        error.to_string(),
    )
    .with_filter(failure)
}

fn filter_apply_failure(settings: &DraftSettings, failure: FilterFailure) -> ApplyFailure {
    ApplyFailure::new(
        FailureCategory::Session,
        Stage::Verification,
        Cause::Generic,
        settings.clone(),
        "filter_verification",
        format!("{:?}", failure.kind),
    )
    .with_filter(failure)
}

fn filter_open_ready(key: AttemptKey, snapshot: &Snapshot) -> bool {
    match snapshot.readiness {
        Some(OpenReadiness::PausedPrepared) => snapshot.playback == InitialPlayback::Paused,
        Some(OpenReadiness::Live) => snapshot.filters.as_ref().is_some_and(|filters| {
            filters.key == FilterAttemptKey { apply: key.apply, attempt: key.attempt, pass: FilterPass::Open }
                && matches!(filters.status, FilterStatus::Confirmed(confirmation) if confirmation.key() == filters.key)
        }),
        None => false,
    }
}

/// Uses existing verifier, never weakens replay predicate or invents missing facts.
fn verified_receipt(
    key: AttemptKey,
    settings: &DraftSettings,
    snapshot: &Snapshot,
) -> Result<Option<OpenReceipt>, ApplyFailure> {
    let Some(readiness) = snapshot.readiness else {
        return Ok(None);
    };
    let Some(session) = snapshot.session.as_ref() else {
        return Ok(None);
    };
    if session.requested.identity != settings.video.identity
        || session.requested.mode != settings.video.mode
    {
        return Err(ApplyFailure::new(
            FailureCategory::Session,
            Stage::Verification,
            Cause::RequestedModeRefused,
            settings.clone(),
            "capture_verification",
            "owner facts do not match supplied prepared settings",
        ));
    }
    let verified = SessionFacts::verify(session.requested.clone(), session.observed.clone())
        .map_err(|error| {
            ApplyFailure::new(
                FailureCategory::Session,
                error.stage,
                error.cause,
                settings.clone(),
                "capture_verification",
                error.to_string(),
            )
            .with_evidence(error.evidence)
        })?;
    let audio = match (&settings.audio, &snapshot.audio) {
        (AudioSelection::Disabled { .. }, AudioAvailability::Disabled) => AudioOutcome::Disabled,
        (AudioSelection::Enabled { source }, AudioAvailability::Active { route })
            if source == route.source()
                && snapshot.audio_epoch == Some(route.epoch())
                && route.generation() == snapshot.generation
                && route.attempt().get() == snapshot.generation.get()
                && route.destination().is_some() =>
        {
            AudioOutcome::Active {
                route: route.clone(),
            }
        }
        (AudioSelection::Enabled { source }, AudioAvailability::Silent { reason }) => {
            AudioOutcome::Silent {
                source: source.clone(),
                reason: reason.clone(),
            }
        }
        (
            _,
            AudioAvailability::Opening { .. }
            | AudioAvailability::Switching { .. }
            | AudioAvailability::Detaching { .. }
            | AudioAvailability::Blocked { .. },
        ) => return Ok(None),
        _ => {
            return Err(ApplyFailure::new(
                FailureCategory::Session,
                Stage::Verification,
                Cause::Generic,
                settings.clone(),
                "audio_verification",
                "owner audio outcome does not match exact requested enabled selection",
            ));
        }
    };
    let filters = match readiness {
        OpenReadiness::PausedPrepared if snapshot.playback == InitialPlayback::Paused => {
            FilterOpenReceipt::PreparedPaused
        }
        OpenReadiness::Live => {
            let Some(filters) = snapshot.filters.as_ref() else {
                return Ok(None);
            };
            if filters.key
                != (FilterAttemptKey {
                    apply: key.apply,
                    attempt: key.attempt,
                    pass: FilterPass::Open,
                })
            {
                return Ok(None);
            }
            match &filters.status {
                FilterStatus::Confirmed(confirmation) if confirmation.key() == filters.key => {
                    FilterOpenReceipt::Confirmed(*confirmation)
                }
                FilterStatus::Failed(failure) => {
                    return Err(filter_apply_failure(settings, (**failure).clone()));
                }
                _ => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(OpenReceipt {
        settings: settings.clone(),
        verification: VerificationSummary {
            captured_fourcc: fact_status(verified.verification.captured_fourcc),
            decoded_size: fact_status(verified.verification.decoded_size),
            nominal_rate: fact_status(verified.verification.nominal_rate),
        },
        audio,
        readiness,
        filters,
    }))
}

fn log_session_event(
    key: Option<AttemptKey>,
    requested: Option<&DraftSettings>,
    event: &SessionEvent,
) {
    let (name, attempt) = match event {
        SessionEvent::FilterResult { key, .. } => ("FilterResult", key.attempt),
        SessionEvent::FilterFault { key, .. } => ("FilterFault", key.attempt),
        SessionEvent::OpenVerified { key, .. } => ("OpenVerified", key.attempt),
        SessionEvent::OpenFailed { key, .. } => ("OpenFailed", key.attempt),
        SessionEvent::SessionFailed { attempt, .. } => ("SessionFailed", *attempt),
        SessionEvent::StreamEnded { attempt, .. } => ("StreamEnded", *attempt),
        SessionEvent::OwnerStopped { attempt, .. } => ("OwnerStopped", *attempt),
        SessionEvent::NativeReleased { attempt } => ("NativeReleased", *attempt),
        SessionEvent::CleanupBlocked { attempt, .. } => ("CleanupBlocked", *attempt),
        SessionEvent::AudioAvailability { attempt, .. } => ("AudioAvailability", *attempt),
        SessionEvent::AudioDetached { attempt, .. } => ("AudioDetached", *attempt),
        SessionEvent::PauseObserved { attempt, .. } => ("PauseObserved", *attempt),
    };
    let failure = match event {
        SessionEvent::OpenFailed { failure, .. }
        | SessionEvent::SessionFailed { failure, .. }
        | SessionEvent::CleanupBlocked { failure, .. } => Some(failure),
        SessionEvent::OwnerStopped {
            outcome: Err(failure),
            ..
        } => Some(failure),
        _ => None,
    };
    let receipt = match event {
        SessionEvent::OpenVerified { receipt, .. } => Some(serde_json::json!({
            "settings": receipt.settings,
            "verification": {
                "captured_fourcc": format!("{:?}", receipt.verification.captured_fourcc),
                "decoded_size": format!("{:?}", receipt.verification.decoded_size),
                "nominal_rate": format!("{:?}", receipt.verification.nominal_rate),
            },
            "audio": match &receipt.audio {
                AudioOutcome::Disabled => serde_json::json!("Disabled"),
                AudioOutcome::Active { route } => serde_json::json!({"Active": {"source": route.source(), "route": route}}),
                AudioOutcome::Silent { source, reason } => serde_json::json!({"Silent": {"source": source, "reason": reason}}),
            },
            "filters": format!("{:?}", receipt.filters),
        })),
        _ => None,
    };
    let value = serde_json::json!({
        "event": name, "apply_id": key.map(|key| key.apply.get()),
        "attempt_id": attempt.get(), "purpose": key.map(|key| key.purpose),
        "requested": requested, "failure": failure, "receipt": receipt,
    });
    tracing::info!(event = %value, "apply_session_event");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::ports::OpenReadiness,
        capture::{apply::fixture_prepared, linux::session_fixture},
        domain::{
            capture::{CaptureMode, CapturedFourCc, FrameRate, FrameSize},
            state::{ApplyId, AttemptPurpose},
        },
        media::controller::{
            BackendEvent, X11WindowId,
            test_support::{Config, Driver},
        },
    };
    use std::sync::mpsc;

    /// Only the deterministic prepared-value port is synthetic. The product,
    /// owner, libmpv transactions and retirement below all use their real paths.
    #[derive(Default)]
    struct FixtureValidator {
        result: Option<crate::app::ports::ValidationResult<PreparedCapture>>,
        requests: Vec<crate::domain::state::ValidationRequest>,
        observation: Option<crate::domain::capture::RecoveryObservation>,
        retired_choices: Vec<crate::domain::capture::SelectionToken>,
        shutdown: bool,
    }
    impl crate::app::ports::DraftValidator for FixtureValidator {
        type Prepared = PreparedCapture;
        fn prepared_settings(prepared: &Self::Prepared) -> &DraftSettings {
            prepared.settings()
        }
        fn prepared_stamp(prepared: &Self::Prepared) -> crate::domain::capture::WatchStamp {
            prepared.stamp()
        }
        fn watch(
            &mut self,
            target: crate::domain::capture::RecoveryWatchTarget,
        ) -> Result<(), crate::app::ports::SubmitFailure> {
            self.observation = Some(crate::domain::capture::RecoveryObservation {
                stamp: crate::domain::capture::WatchStamp {
                    watch: target.watch,
                    epoch: crate::domain::capture::ObservationEpoch::new(1).unwrap(),
                },
                video: crate::domain::capture::VideoPresence::Present,
                audio: if target.audio.enabled() {
                    crate::domain::capture::SourcePresence::Present
                } else {
                    crate::domain::capture::SourcePresence::Disabled
                },
                last_video_removal: None,
            });
            Ok(())
        }
        fn poll_recovery(&mut self) -> Option<crate::domain::capture::RecoveryObservation> {
            self.observation.take()
        }
        fn clear_watch(&mut self) {
            self.observation = None;
        }
        fn retire_selection(&mut self, token: crate::domain::capture::SelectionToken) {
            self.retired_choices.push(token);
        }
        fn begin_validate(
            &mut self,
            request: crate::domain::state::ValidationRequest,
        ) -> Result<(), crate::app::ports::SubmitFailure> {
            if self.shutdown {
                return Err(crate::app::ports::SubmitFailure::Disconnected);
            }
            if self.result.is_some() {
                return Err(crate::app::ports::SubmitFailure::CapacityUnavailable);
            }
            self.requests.push(request.clone());
            self.result = Some(crate::app::ports::ValidationResult {
                stamp: request.watch,
                result: match crate::capture::apply::fixture_prepared_at(
                    request.settings.clone(),
                    request.watch,
                ) {
                    Ok(prepared) => crate::app::ports::ValidationOutcome::Prepared(prepared),
                    Err(error) => crate::app::ports::ValidationOutcome::Failed(error),
                },
                request,
            });
            Ok(())
        }
        fn poll_validation(
            &mut self,
        ) -> Option<crate::app::ports::ValidationResult<Self::Prepared>> {
            self.result.take()
        }
        fn cancel_validation(&mut self, _: crate::domain::state::ValidationKey) {
            // The accepted terminal result still drains through app.poll.
        }
        fn shutdown(&mut self) {
            self.shutdown = true;
        }
        fn shutdown_complete(&mut self) -> bool {
            self.shutdown && self.result.is_none()
        }
    }

    type FixtureApp = crate::app::apply::ApplyCoordinator<FixtureValidator, GateRunner>;

    fn fixture_wait(
        app: &mut FixtureApp,
        transition: &str,
        mut completed: impl FnMut(&mut FixtureApp) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            app.poll();
            if completed(app) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                let report = app.runner_mut().report.clone();
                panic!(
                    "fixture {transition} timed out: product={:?}, failures={:?}, report={report}",
                    app.model().state_identity(),
                    app.model().failures()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    #[ignore = "requires frozen libmpv and FURAMI_PLAYBACK_FIXTURE"]
    fn real_libmpv_guarded_resume() {
        use crate::{
            domain::state::{PlaybackState, ProductPhase},
            media::ffi::{FixtureBackend, FixtureMilestone, FixtureRecorder},
        };
        use std::{
            cell::RefCell,
            panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
            rc::Rc,
        };

        let prefix = std::env::var("FURAMI_MEDIA_PREFIX").expect("frozen FURAMI_MEDIA_PREFIX");
        let fixture = std::path::PathBuf::from(
            std::env::var_os("FURAMI_PLAYBACK_FIXTURE").expect("FURAMI_PLAYBACK_FIXTURE"),
        );
        assert!(
            fixture.is_absolute() && fixture.is_file(),
            "absolute existing fixture required"
        );
        let fixture = fixture.canonicalize().unwrap();
        let recorder = FixtureRecorder::default();
        let owner_recorder = recorder.clone();
        // Spawning happens on the app thread; the recorder alone crosses threads.
        let opens = Rc::new(RefCell::new(Vec::new()));
        let owner_opens = Rc::clone(&opens);
        let capabilities = super::super::filter_catalog::query_qualified_capabilities(
            std::path::Path::new(&prefix),
        )
        .expect("frozen qualified filter catalog");
        let runner = GateRunner::with_spawner(Ok(capabilities), move |generation, config| {
            let requested = config.video.requested();
            let input = config
                .video
                .validate_snapshot(&session_fixture(&["/dev/video0"], requested.mode))
                .map_err(|error| MediaError::new("fixture_input", error.to_string()))?;
            let requested = input.requested().clone();
            owner_opens
                .borrow_mut()
                .push((generation, requested.clone(), config.gain));
            let prefix = prefix.clone();
            let fixture = fixture.clone();
            let recorder = owner_recorder.clone();
            OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || {
                FixtureBackend::new_with_filters(
                    prefix,
                    fixture,
                    input,
                    config.gain,
                    config.playback,
                    config.watch,
                    config.filters,
                    config.compiled_filters,
                    config.filter_key,
                    Some(recorder),
                )
                .expect("validated fixture constructor on real owner thread")
            })
        });
        let mut applied = settings();
        applied.video.mode.size = FrameSize::new(320, 240).unwrap();
        applied.video.mode.rate = FrameRate::new(30, 1).unwrap();
        let mut app = FixtureApp::new(
            applied.clone(),
            PlaybackGain::default(),
            FixtureValidator::default(),
            runner,
        );
        // Error/assertion/timeout paths still request Quit and drain genuine owners.
        let result = catch_unwind(AssertUnwindSafe(|| {
            app.apply(app.model().state_identity(), app.model().draft().revision)
                .unwrap();
            fixture_wait(&mut app, "initial validation", |app| {
                app.model().opening().is_some()
            });
            let first_update = app.runner_mut().take_native_update();
            assert!(first_update.create_native && !first_update.release_native);
            let first = first_update.attempt.unwrap();
            app.runner_mut().surface_ready(token(first.get()));
            fixture_wait(&mut app, "initial real playback", |app| {
                app.model().active().is_some()
            });
            assert_eq!(
                app.model().active().unwrap().playback(),
                PlaybackState::Live
            );
            let mut draft = applied.clone();
            draft.video.mode.rate = FrameRate::new(15, 1).unwrap();
            let revision = app
                .edit_draft(app.model().draft().revision, draft.clone())
                .unwrap();
            let paused_gain = PlaybackGain::new(80, true).unwrap();
            assert_eq!(app.set_gain(paused_gain), SubmitStatus::Accepted);
            assert_eq!(app.toggle_pause(first).unwrap(), SubmitStatus::Accepted);
            assert!(matches!(
                app.model().active().unwrap().playback(),
                PlaybackState::PausePending { .. }
            ));
            fixture_wait(&mut app, "correlated real pause", |app| {
                app.model()
                    .active()
                    .is_some_and(|active| active.playback() == PlaybackState::Paused)
            });
            app.resume(app.model().state_identity(), first).unwrap();
            fixture_wait(&mut app, "genuine first owner destruction", |app| {
                app.runner_mut().phase() == GatePhase::Releasing
            });
            assert_eq!(app.model().phase(), ProductPhase::ClosingResume);
            assert!(app.runner_mut().endpoint.is_none());
            assert!(
                app.runner_mut().owner_event.is_none(),
                "OwnerStopped consumed by actual coordinator"
            );
            let release = app.runner_mut().take_native_update();
            assert!(release.release_native && !release.create_native);
            assert_eq!(release.attempt, Some(first));
            assert!(matches!(recorder.snapshot().unwrap().last(),
                Some(FixtureMilestone::Destroyed { generation }) if generation.get() == first.get()));

            // Withhold only the modeled native host release after actual destruction.
            let gap_gain = PlaybackGain::new(83, false).unwrap();
            assert_eq!(app.set_gain(gap_gain), SubmitStatus::Accepted);
            for _ in 0..3 {
                app.poll();
            }
            assert_eq!(opens.borrow().len(), 1, "no owner before native release");
            assert_eq!(app.gain(), gap_gain);
            assert_eq!(app.model().draft().settings, draft);
            app.runner_mut().native_released(first);
            fixture_wait(&mut app, "single fresh resume opening", |app| {
                app.model().opening().is_some()
            });
            let (replacement, target) = app.model().opening().unwrap();
            assert_eq!(replacement.purpose, AttemptPurpose::Resume);
            assert_eq!(target, &applied);
            assert_ne!(replacement.attempt, first);
            let second_update = app.runner_mut().take_native_update();
            assert!(second_update.create_native && !second_update.release_native);
            assert_eq!(second_update.attempt, Some(replacement.attempt));
            app.runner_mut()
                .surface_ready(token(replacement.attempt.get()));
            fixture_wait(&mut app, "fresh real Live playback", |app| {
                app.model()
                    .active()
                    .is_some_and(|active| active.attempt() == replacement.attempt)
            });
            assert_eq!(
                app.model().active().unwrap().playback(),
                PlaybackState::Live
            );
            assert_eq!(app.model().active().unwrap().applied().settings(), &applied);
            assert_eq!(app.model().draft().revision, revision);
            assert_eq!(app.model().draft().settings, draft);
            assert_eq!(app.gain(), gap_gain);
            assert_eq!(app.validator_mut().requests.len(), 2);
            let actual_opens = opens.borrow();
            assert_eq!(actual_opens.len(), 2);
            for (_, facts, _) in actual_opens.iter() {
                assert_eq!(facts.identity, applied.video.identity);
                assert_eq!(facts.mode, applied.video.mode);
            }
            assert_eq!(actual_opens[1].2, gap_gain);
            drop(actual_opens);
            assert_eq!(
                recorder.snapshot().unwrap(),
                vec![
                    FixtureMilestone::Created {
                        generation: Generation::new(first.get()).unwrap()
                    },
                    FixtureMilestone::Initialized {
                        generation: Generation::new(first.get()).unwrap(),
                        gain: PlaybackGain::default()
                    },
                    FixtureMilestone::Destroyed {
                        generation: Generation::new(first.get()).unwrap()
                    },
                    FixtureMilestone::Created {
                        generation: Generation::new(replacement.attempt.get()).unwrap()
                    },
                    FixtureMilestone::Initialized {
                        generation: Generation::new(replacement.attempt.get()).unwrap(),
                        gain: gap_gain
                    },
                ],
                "destroy precedes replacement initialization; fresh init checks pause=no"
            );
        }));
        app.quit();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !app.model().shutdown_ready() && std::time::Instant::now() < deadline {
            app.poll();
            let update = app.runner_mut().take_native_update();
            // A failed assertion may have consumed the held host effect already.
            // Releasing still proves the real ack was consumed; never invent it.
            let release_pending = app.runner_mut().phase() == GatePhase::Releasing
                && app.runner_mut().endpoint.is_none()
                && app.runner_mut().owner_event.is_none();
            if update.release_native || release_pending {
                assert!(app.runner_mut().endpoint.is_none());
                assert!(app.runner_mut().owner_event.is_none());
                app.runner_mut().native_released(update.attempt.unwrap());
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            app.model().shutdown_ready(),
            "Quit must drain every genuine owner: {:?}",
            app.model().state_identity()
        );
        let milestones = recorder.snapshot().unwrap();
        assert_eq!(
            milestones
                .iter()
                .filter(|entry| matches!(entry, FixtureMilestone::Created { .. }))
                .count(),
            milestones
                .iter()
                .filter(|entry| matches!(entry, FixtureMilestone::Destroyed { .. }))
                .count()
        );
        if let Err(payload) = result {
            resume_unwind(payload);
        }
        assert_eq!(opens.borrow().len(), 2, "Quit cannot reopen");
        eprintln!("real guarded resume milestones: {milestones:?}");
        eprintln!(
            "finite null-output owner/coordinator qualification only; no live freshness, audible silence or Qt/XID proof"
        );
    }

    fn settings() -> DraftSettings {
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(1280, 720).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        DraftSettings {
            filters: crate::domain::filters::FilterChain::default(),
            video: crate::domain::capture::ModeRequest {
                identity: session_fixture(&["/dev/video0"], mode).devices()[0]
                    .identity()
                    .clone(),
                mode,
            },
            audio: AudioSelection::default(),
        }
    }
    fn key(value: u64) -> AttemptKey {
        AttemptKey {
            apply: ApplyId::new(1).unwrap(),
            attempt: AttemptId::new(value).unwrap(),
            purpose: AttemptPurpose::Candidate,
        }
    }
    fn token(value: u64) -> SurfaceToken {
        SurfaceToken {
            generation: Generation::new(value).unwrap(),
            xid: X11WindowId::new(71).unwrap(),
        }
    }
    fn pause_intent() -> ImmediateIntent {
        ImmediateIntent::SetPaused {
            request: PauseRequestId::new(1).unwrap(),
            paused: true,
        }
    }
    fn runner(config: Config) -> (GateRunner, mpsc::Receiver<Driver>) {
        let (tx, rx) = mpsc::channel();
        let mut config = Some(config);
        let runner = GateRunner::with_spawner(
            Ok(crate::media::filter_catalog::fixture_capabilities()),
            move |generation, session| {
                let mode = session.video.requested().mode;
                let input = session
                    .video
                    .validate_snapshot(&session_fixture(&["/dev/video0"], mode))
                    .unwrap();
                let requested = input.requested().clone();
                let (driver, backend) = Driver::pair(config.take().unwrap_or_default());
                let backend =
                    backend.with_open_filters(session.filter_key, session.compiled_filters);
                tx.send(driver).unwrap();
                OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || backend)
            },
        );
        (runner, rx)
    }
    fn output_fixture_plan() -> OutputPlan {
        OutputPlan::Silent {
            revision: crate::domain::output::OutputRevision::first(),
            reason: crate::domain::output::OutputSilence::NoAvailableOutput,
        }
    }
    fn prepared_filters(settings: &DraftSettings) -> CompiledFilterChain {
        compile_chain(
            &settings.filters,
            &super::super::filter_catalog::fixture_capabilities(),
        )
        .unwrap()
    }
    fn open(runner: &mut GateRunner, value: u64) {
        runner
            .begin_open(
                key(value),
                fixture_prepared(settings()).unwrap(),
                prepared_filters(&settings()),
                PlaybackGain::default(),
                InitialPlayback::Live,
                output_fixture_plan(),
            )
            .unwrap();
        assert!(runner.take_native_update().create_native);
    }
    fn ready(runner: &mut GateRunner, driver: &Driver, value: u64) {
        runner.surface_ready(token(value));
        driver.initialized.recv().unwrap();
        let (load, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        driver.send(BackendEvent::PlaybackRestart);
        driver.confirm_open_filters();
        driver.fence();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match runner.poll() {
                Some(SessionEvent::OpenVerified { key: received, .. }) => {
                    assert_eq!(received, key(value));
                    break;
                }
                Some(SessionEvent::OpenFailed { failure, .. }) => panic!("{failure}"),
                _ => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "owner did not publish verified readiness"
                    );
                    std::thread::yield_now();
                }
            }
        }
    }
    #[test]
    fn pause_event_is_correlated_once_per_attempt_and_generic_status_is_not_presentation() {
        let (mut runner, drivers) = runner(Config::default());
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        ready(&mut runner, &driver, 1);
        let request = PauseRequestId::new(1).unwrap();
        let mut progress = snapshot(&settings(), AudioAvailability::Disabled);
        progress.pause = Some(super::super::controller::PauseObservation {
            request: None,
            paused: true,
        });
        assert!(runner.snapshot(progress.clone()).is_none());
        runner.submit_immediate(
            key(1).attempt,
            ImmediateIntent::SetPaused {
                request,
                paused: true,
            },
        );
        progress.pause = Some(super::super::controller::PauseObservation {
            request: Some(PauseRequestId::new(2).unwrap()),
            paused: true,
        });
        assert!(runner.snapshot(progress.clone()).is_none());
        progress.pause = Some(super::super::controller::PauseObservation {
            request: Some(request),
            paused: true,
        });
        assert_eq!(
            runner.snapshot(progress.clone()),
            Some(SessionEvent::PauseObserved {
                attempt: key(1).attempt,
                request,
                paused: true
            })
        );
        assert!(runner.snapshot(progress.clone()).is_none());
        runner.stop(key(1).attempt, StopReason::Close);
        assert!(runner.snapshot(progress.clone()).is_none());
        stopped(&mut runner);
        driver.destroyed.recv().unwrap();
        runner.native_released(key(1).attempt);
        assert_eq!(
            runner.poll(),
            Some(SessionEvent::NativeReleased {
                attempt: key(1).attempt
            })
        );
        open(&mut runner, 2);
        let second = drivers.recv().unwrap();
        ready(&mut runner, &second, 2);
        runner.submit_immediate(
            key(2).attempt,
            ImmediateIntent::SetPaused {
                request,
                paused: true,
            },
        );
        assert!(runner.snapshot(progress.clone()).is_none());
        progress.generation = Generation::new(2).unwrap();
        assert_eq!(
            runner.snapshot(progress),
            Some(SessionEvent::PauseObserved {
                attempt: key(2).attempt,
                request,
                paused: true
            })
        );
        runner.stop(key(2).attempt, StopReason::Close);
        stopped(&mut runner);
        second.destroyed.recv().unwrap();
    }

    #[test]
    fn ended_snapshot_cannot_emit_pause_after_terminal_event() {
        let (mut runner, drivers) = runner(Config::default());
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        ready(&mut runner, &driver, 1);
        let request = PauseRequestId::new(1).unwrap();
        runner.submit_immediate(
            key(1).attempt,
            ImmediateIntent::SetPaused {
                request,
                paused: true,
            },
        );
        let mut progress = snapshot(&settings(), AudioAvailability::Disabled);
        progress.pause = Some(super::super::controller::PauseObservation {
            request: Some(request),
            paused: true,
        });
        progress.stream_ended = Some((0, 0));
        progress.readiness = None;
        assert_eq!(
            runner.snapshot(progress.clone()),
            Some(SessionEvent::StreamEnded {
                attempt: key(1).attempt,
                reason: 0,
                error: 0,
            })
        );
        assert!(runner.snapshot(progress).is_none());
        runner.stop(key(1).attempt, StopReason::Close);
        stopped(&mut runner);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn snapshot_failure_and_real_owner_stop_outrank_correlated_pause() {
        for failed_snapshot in [true, false] {
            let (mut runner, drivers) = runner(Config::default());
            open(&mut runner, 1);
            let driver = drivers.recv().unwrap();
            ready(&mut runner, &driver, 1);
            let request = PauseRequestId::new(1).unwrap();
            runner.submit_immediate(
                key(1).attempt,
                ImmediateIntent::SetPaused {
                    request,
                    paused: true,
                },
            );
            if failed_snapshot {
                let mut progress = snapshot(&settings(), AudioAvailability::Disabled);
                progress.pause = Some(super::super::controller::PauseObservation {
                    request: Some(request),
                    paused: true,
                });
                progress.failure = Some(MediaError::new("pause_property", "failed read"));
                assert!(matches!(
                    runner.snapshot(progress),
                    Some(SessionEvent::SessionFailed { .. })
                ));
            } else {
                runner
                    .endpoint
                    .as_ref()
                    .unwrap()
                    .stop(Generation::new(1).unwrap(), None);
            }
            runner.endpoint.as_mut().unwrap().wait_for_ack().unwrap();
            while let Some(event) = runner.poll() {
                assert!(!matches!(event, SessionEvent::PauseObserved { .. }));
            }
            assert!(runner.take_native_update().release_native);
            driver.destroyed.recv().unwrap();
        }
    }
    #[test]
    fn gain_is_admitted_during_opening_before_surface_but_pause_requires_ready() {
        let (mut runner, drivers) = runner(Config::default());
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        let gain = PlaybackGain::new(80, true).unwrap();
        assert_eq!(
            runner.submit_immediate(key(1).attempt, ImmediateIntent::SetGain(gain)),
            SubmitStatus::Accepted
        );
        assert_eq!(
            runner.submit_immediate(key(1).attempt, pause_intent()),
            SubmitStatus::NotReady
        );
        assert_eq!(
            runner.submit_immediate(key(2).attempt, ImmediateIntent::SetGain(gain)),
            SubmitStatus::StaleGeneration
        );
        runner.surface_ready(token(1));
        driver.initialized.recv().unwrap();
        let (load, command) = driver.submitted.recv().unwrap();
        assert_eq!(command, super::super::controller::BackendCommand::LoadInput);
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        assert_eq!(
            driver.submitted.recv().unwrap().1,
            super::super::controller::BackendCommand::SetGain(gain)
        );
        runner.stop(key(1).attempt, StopReason::Close);
        assert_eq!(
            runner.submit_immediate(
                key(1).attempt,
                ImmediateIntent::SetGain(PlaybackGain::default())
            ),
            SubmitStatus::Closing
        );
        stopped(&mut runner);
        driver.destroyed.recv().unwrap();
    }

    fn stopped(runner: &mut GateRunner) {
        runner.endpoint.as_mut().unwrap().wait_for_ack().unwrap();
        for _ in 0..4 {
            if matches!(runner.poll(), Some(SessionEvent::OwnerStopped { .. })) {
                return;
            }
        }
        panic!("genuine acknowledgement not delivered");
    }
    #[test]
    fn handoff_once_stale_or_duplicate_publication_never_reattaches() {
        let (mut runner, drivers) = runner(Config::default());
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        runner.surface_ready(token(2));
        runner.surface_ready(token(1));
        driver.initialized.recv().unwrap();
        runner.surface_ready(token(1));
        assert!(driver.initialized.try_recv().is_err());
        assert_eq!(
            runner.submit_immediate(key(1).attempt, pause_intent()),
            SubmitStatus::NotReady
        );
        runner.stop(key(1).attempt, StopReason::Close);
        stopped(&mut runner);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn ack_precedes_coalesced_ready_and_old_host_release_never_reopens() {
        let (mut runner, drivers) = runner(Config::default());
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        runner.surface_ready(token(1));
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        runner.stop(key(1).attempt, StopReason::Replace);
        stopped(&mut runner);
        let update = runner.take_native_update();
        assert!(update.release_native && !update.create_native);
        assert!(
            runner
                .begin_open(
                    key(2),
                    fixture_prepared(settings()).unwrap(),
                    prepared_filters(&settings()),
                    PlaybackGain::default(),
                    InitialPlayback::Live,
                    output_fixture_plan(),
                )
                .is_err()
        );
        driver.destroyed.recv().unwrap();
        runner.native_released(key(1).attempt);
        assert!(matches!(
            runner.poll(),
            Some(SessionEvent::NativeReleased { .. })
        ));
        assert!(!runner.take_native_update().create_native);
        assert!(drivers.try_recv().is_err());
        open(&mut runner, 2);
        let second = drivers.recv().unwrap();
        runner.stop(key(2).attempt, StopReason::Close);
        stopped(&mut runner);
        second.destroyed.recv().unwrap();
    }
    #[test]
    fn close_before_publication_then_reopen_uses_distinct_attempt() {
        let (mut runner, drivers) = runner(Config::default());
        open(&mut runner, 1);
        let first = drivers.recv().unwrap();
        runner.stop(key(1).attempt, StopReason::Close);
        runner.surface_ready(token(1));
        stopped(&mut runner);
        first.destroyed.recv().unwrap();
        assert!(first.initialized.try_recv().is_err());
        runner.take_native_update();
        runner.native_released(key(1).attempt);
        runner.poll();
        open(&mut runner, 2);
        let second = drivers.recv().unwrap();
        ready(&mut runner, &second, 2);
        assert_eq!(
            runner.submit_immediate(key(1).attempt, pause_intent()),
            SubmitStatus::StaleGeneration
        );
        runner.stop(key(2).attempt, StopReason::Close);
        stopped(&mut runner);
        second.destroyed.recv().unwrap();
    }
    #[test]
    fn close_during_initialization_or_pending_load_never_revives_ready() {
        for held in [true, false] {
            let (mut runner, drivers) = runner(Config {
                hold_initialize: held,
                ..Config::default()
            });
            open(&mut runner, 1);
            let driver = drivers.recv().unwrap();
            runner.surface_ready(token(1));
            driver.initialized.recv().unwrap();
            if !held {
                driver.submitted.recv().unwrap();
            }
            runner.stop(key(1).attempt, StopReason::Close);
            if held {
                driver.initialize_release.send(()).unwrap();
            }
            stopped(&mut runner);
            assert_eq!(runner.phase(), GatePhase::Releasing);
            if held {
                assert!(driver.submitted.try_recv().is_err());
            }
            driver.destroyed.recv().unwrap();
        }
    }
    #[test]
    fn failed_open_destroys_partial_handle_once_before_native_release() {
        for creates_handle in [false, true] {
            let (mut runner, drivers) = runner(Config {
                creates_handle,
                hold_shutdown: true,
                initialization_error: Some(MediaError::new(
                    "initialization",
                    "injected open failure",
                )),
                ..Config::default()
            });
            open(&mut runner, 1);
            let driver = drivers.recv().unwrap();
            runner.surface_ready(token(1));
            driver.initialized.recv().unwrap();
            driver.shutdown_started.recv().unwrap();
            assert!(matches!(
                runner.poll(),
                Some(SessionEvent::OpenFailed { .. })
            ));
            assert!(!runner.take_native_update().release_native);
            driver.shutdown_release.send(()).unwrap();
            stopped(&mut runner);
            assert_eq!(driver.destroyed.recv().unwrap(), creates_handle);
            assert!(driver.destroyed.try_recv().is_err());
            assert!(runner.take_native_update().release_native);
        }
    }
    #[test]
    fn no_resource_spawn_failure_has_no_ack_or_native_effect() {
        let mut runner = GateRunner::with_spawner(
            Ok(crate::media::filter_catalog::fixture_capabilities()),
            |_, _| Err(MediaError::new("owner_spawn", "injected spawn failure")),
        );
        assert!(matches!(
            runner.begin_open(
                key(1),
                fixture_prepared(settings()).unwrap(),
                prepared_filters(&settings()),
                PlaybackGain::default(),
                InitialPlayback::Live,
                output_fixture_plan(),
            ),
            Err(StartFailure::NoResourcesCreated(_))
        ));
        let update = runner.take_native_update();
        assert!(!update.create_native && !update.release_native);
        assert!(runner.endpoint.is_none());
        assert!(runner.poll().is_none());
        assert_eq!(
            runner.stop(key(1).attempt, StopReason::Failed),
            StopSubmission::NoResourcesCreated
        );
    }
    #[test]
    fn actual_audio_quiescence_precedes_video_destroy_and_native_release() {
        let (mut runner, drivers) = runner(Config {
            hold_quiesce: true,
            creates_handle: true,
            ..Config::default()
        });
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        ready(&mut runner, &driver, 1);
        runner.stop(key(1).attempt, StopReason::Replace);
        driver.quiesce_started.recv().unwrap();
        assert!(driver.destroyed.try_recv().is_err());
        assert!(!runner.take_native_update().release_native);
        assert!(runner.poll().is_none());
        assert!(
            runner
                .begin_open(
                    key(2),
                    fixture_prepared(settings()).unwrap(),
                    prepared_filters(&settings()),
                    PlaybackGain::default(),
                    InitialPlayback::Live,
                    output_fixture_plan(),
                )
                .is_err()
        );
        driver.quiesce_release.send(()).unwrap();
        driver.destroyed.recv().unwrap();
        stopped(&mut runner);
        assert!(runner.take_native_update().release_native);
    }
    #[test]
    fn command_overflow_visible_before_destruction_ack_and_level_stop_survives_full_queue() {
        let (mut runner, drivers) = runner(Config {
            hold_shutdown: true,
            creates_handle: true,
            ..Config::default()
        });
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        ready(&mut runner, &driver, 1);
        // Readiness completes the load reply, so the owner can dequeue one
        // command. Observe that real submission and withhold its reply before
        // filling the 64 waiting slots; scheduling cannot change their count.
        assert_eq!(
            runner.submit_immediate(key(1).attempt, pause_intent()),
            SubmitStatus::Accepted
        );
        assert!(matches!(
            driver.submitted.recv().unwrap().1,
            super::super::controller::BackendCommand::SetPaused { .. }
        ));
        for _ in 0..64 {
            assert_eq!(
                runner.submit_immediate(key(1).attempt, pause_intent()),
                SubmitStatus::Accepted
            );
        }
        // Gain has its own latest-value mailbox, not a discrete queue slot.
        assert_eq!(
            runner.submit_immediate(
                key(1).attempt,
                ImmediateIntent::SetGain(PlaybackGain::new(80, true).unwrap()),
            ),
            SubmitStatus::Accepted
        );
        driver.fence();
        assert!(driver.submitted.try_recv().is_err());
        assert_eq!(
            runner.submit_immediate(key(1).attempt, pause_intent()),
            SubmitStatus::CapacityExceeded
        );
        assert!(matches!(
            runner.poll(),
            Some(SessionEvent::SessionFailed { attempt, failure })
                if attempt == key(1).attempt
                    && failure.category == FailureCategory::Session
                    && failure.requested.as_ref() == &settings()
        ));
        driver.shutdown_started.recv().unwrap();
        assert!(driver.destroyed.try_recv().is_err());
        assert!(runner.poll().is_none());
        assert!(!runner.take_native_update().release_native);
        assert_eq!(
            runner.submit_immediate(key(1).attempt, pause_intent()),
            SubmitStatus::Closing
        );
        driver.shutdown_release.send(()).unwrap();
        stopped(&mut runner);
        driver.destroyed.recv().unwrap();
        assert!(runner.take_native_update().release_native);
        assert!(driver.submitted.try_recv().is_err());
    }
    #[test]
    fn forced_surface_loss_poison_blocks_restore_even_after_real_cleanup() {
        let (mut runner, drivers) = runner(Config {
            hold_shutdown: true,
            ..Config::default()
        });
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        ready(&mut runner, &driver, 1);
        runner.surface_lost(key(1).attempt);
        assert!(matches!(
            runner.poll(),
            Some(SessionEvent::SessionFailed { .. })
        ));
        assert!(matches!(
            runner.poll(),
            Some(SessionEvent::CleanupBlocked { .. })
        ));
        assert!(!runner.take_native_update().release_native);
        driver.shutdown_release.send(()).unwrap();
        assert!(runner.wait_for_owner_ack(key(1).attempt).is_ok());
        stopped(&mut runner);
        driver.destroyed.recv().unwrap();
        runner.take_native_update();
        runner.native_released(key(1).attempt);
        runner.poll();
        assert!(
            runner
                .begin_open(
                    key(2),
                    fixture_prepared(settings()).unwrap(),
                    prepared_filters(&settings()),
                    PlaybackGain::default(),
                    InitialPlayback::Live,
                    output_fixture_plan(),
                )
                .is_err()
        );
    }
    #[test]
    fn missing_owner_ack_never_releases_native_or_allows_next_owner() {
        let mut runner = GateRunner::with_spawner(
            Ok(crate::media::filter_catalog::fixture_capabilities()),
            |generation, _| {
                OwnerEndpoint::spawn_with_backend(
                    generation,
                    || -> crate::media::controller::test_support::FakeBackend {
                        panic!("injected factory panic")
                    },
                )
            },
        );
        open(&mut runner, 1);
        runner.surface_lost(key(1).attempt);
        assert!(runner.wait_for_owner_ack(key(1).attempt).is_err());
        for _ in 0..4 {
            runner.poll();
        }
        assert!(!runner.take_native_update().release_native);
        runner.native_released(key(1).attempt);
        assert_eq!(runner.attempt(), Some(key(1).attempt));
    }
    fn snapshot(settings: &DraftSettings, audio: AudioAvailability) -> Snapshot {
        let input = fixture_prepared(settings.clone()).unwrap();
        let facts = SessionFacts::verify(
            input.selection().requested(),
            super::super::session::ObservedFacts::default(),
        )
        .unwrap();
        Snapshot {
            filters: Some(FilterSnapshot {
                key: FilterAttemptKey {
                    apply: key(1).apply,
                    attempt: key(1).attempt,
                    pass: FilterPass::Open,
                },
                sequence: 1,
                status: FilterStatus::Confirmed(
                    crate::domain::state::FilterConfirmation::checked(
                        FilterAttemptKey {
                            apply: key(1).apply,
                            attempt: key(1).attempt,
                            pass: FilterPass::Open,
                        },
                        0.0,
                        2.0,
                        32,
                    )
                    .unwrap(),
                ),
            }),
            generation: Generation::new(1).unwrap(),
            initialized: true,
            file_loaded: true,
            playback_started: true,
            session: Some(facts),
            pause: None,
            stream_ended: None,
            readiness: Some(OpenReadiness::Live),
            load_complete: true,
            playback: InitialPlayback::Live,
            audio_detached: None,
            audio_epoch: None,
            failure: None,
            audio,
        }
    }
    #[test]
    fn receipt_requires_session_facts_and_exact_enabled_audio_outcome() {
        let settings = settings();
        let mut snapshot = snapshot(&settings, AudioAvailability::Disabled);
        let receipt = verified_receipt(key(1), &settings, &snapshot)
            .unwrap()
            .unwrap();
        assert!(receipt.matches(&settings));
        assert_eq!(receipt.verification.captured_fourcc, FactStatus::Unverified);
        let source =
            crate::domain::capture::AudioSourceIdentity::new("explicit".into(), Vec::new())
                .unwrap();
        let enabled = DraftSettings {
            audio: AudioSelection::Enabled {
                source: source.clone(),
            },
            ..settings.clone()
        };
        assert!(verified_receipt(key(1), &enabled, &snapshot).is_err());
        snapshot.audio = AudioAvailability::Opening {
            epoch: AudioEpoch::new(1).unwrap(),
        };
        assert!(
            verified_receipt(key(1), &enabled, &snapshot)
                .unwrap()
                .is_none()
        );
        let route = super::super::loopback::LoopbackReceipt::for_test(
            snapshot.generation,
            AttemptId::new(snapshot.generation.get()).unwrap(),
            AudioEpoch::new(1).unwrap(),
            crate::domain::capture::WatchStamp {
                watch: crate::domain::capture::WatchId::new(1).unwrap(),
                epoch: crate::domain::capture::ObservationEpoch::new(1).unwrap(),
            },
            source.clone(),
            crate::domain::output::LiveSinkTarget::new(
                crate::domain::output::SinkIdentity::new("fixture.output".into(), vec![]).unwrap(),
                std::num::NonZeroU64::new(20).unwrap(),
                10,
            )
            .unwrap(),
            crate::domain::output::OutputRevision::first(),
        );
        snapshot.audio_epoch = Some(route.epoch());
        let wrong_source = super::super::loopback::LoopbackReceipt::for_test(
            route.generation(),
            route.attempt(),
            route.epoch(),
            route.watch(),
            crate::domain::capture::AudioSourceIdentity::new("wrong".into(), vec![]).unwrap(),
            route.destination().unwrap().clone(),
            route.output_revision(),
        );
        snapshot.audio = AudioAvailability::Active {
            route: wrong_source,
        };
        assert!(verified_receipt(key(1), &enabled, &snapshot).is_err());
        snapshot.audio = AudioAvailability::Active {
            route: route.clone(),
        };
        assert_eq!(
            verified_receipt(key(1), &enabled, &snapshot)
                .unwrap()
                .unwrap()
                .audio,
            AudioOutcome::Active { route }
        );
        snapshot.session = None;
        assert!(
            verified_receipt(key(1), &enabled, &snapshot)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn contradictions_and_wrong_prepared_settings_never_get_receipt() {
        let settings = settings();
        let mut snapshot = snapshot(&settings, AudioAvailability::Disabled);
        snapshot.session.as_mut().unwrap().observed.nominal_rate =
            Some(super::super::session::Observation {
                value: 60.0,
                source: super::super::session::Source::MpvConfiguredContainerFps,
            });
        snapshot.session.as_mut().unwrap().observed.decoded_size =
            Some(super::super::session::Observation {
                value: FrameSize::new(1920, 1080).unwrap(),
                source: super::super::session::Source::MpvDecodedParams,
            });
        assert_eq!(
            verified_receipt(key(1), &settings, &snapshot)
                .unwrap_err()
                .stage,
            Stage::Verification
        );
        let mut different = settings.clone();
        different.video.mode.rate = FrameRate::new(30, 1).unwrap();
        assert!(verified_receipt(key(1), &different, &snapshot).is_err());
    }
    #[test]
    fn missing_facts_stay_unverified_and_nominal_report_approximate() {
        let settings = settings();
        let mut snapshot = snapshot(&settings, AudioAvailability::Disabled);
        let receipt = verified_receipt(key(1), &settings, &snapshot)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.verification.decoded_size, FactStatus::Unverified);
        assert_eq!(receipt.verification.nominal_rate, FactStatus::Unverified);
        snapshot.session.as_mut().unwrap().observed.nominal_rate =
            Some(super::super::session::Observation {
                value: 60.0,
                source: super::super::session::Source::MpvContainerFps,
            });
        assert_eq!(
            verified_receipt(key(1), &settings, &snapshot)
                .unwrap()
                .unwrap()
                .verification
                .nominal_rate,
            FactStatus::Approximate
        );
    }

    #[test]
    fn configured_fractional_rate_receipt_keeps_exact_requested_settings() {
        for numerator in [60_000, 30_000] {
            let mut settings = settings();
            settings.video.mode.rate = FrameRate::new(numerator, 1001).unwrap();
            let mut snapshot = snapshot(&settings, AudioAvailability::Disabled);
            snapshot.session.as_mut().unwrap().observed.nominal_rate =
                Some(super::super::session::Observation {
                    value: f64::from(numerator) / 1001.0,
                    source: super::super::session::Source::MpvConfiguredContainerFps,
                });
            let receipt = verified_receipt(key(1), &settings, &snapshot)
                .unwrap()
                .unwrap();
            assert!(receipt.matches(&settings));
            assert_eq!(receipt.settings.video.mode.rate.numerator(), numerator);
            assert_eq!(receipt.settings.video.mode.rate.denominator(), 1001);
            assert_eq!(receipt.verification.nominal_rate, FactStatus::Configured);
            assert_eq!(receipt.verification.captured_fourcc, FactStatus::Unverified);
        }
    }

    #[test]
    fn silent_enabled_and_prepared_paused_receipts_preserve_exact_desired_source() {
        let source =
            crate::domain::capture::AudioSourceIdentity::new("selected".into(), vec![]).unwrap();
        let enabled = DraftSettings {
            audio: AudioSelection::Enabled {
                source: source.clone(),
            },
            ..settings()
        };
        let mut progress = snapshot(
            &enabled,
            AudioAvailability::Silent {
                reason: AudioSilence::Paused,
            },
        );
        progress.playback = InitialPlayback::Paused;
        progress.playback_started = false;
        progress.pause = Some(super::super::controller::PauseObservation {
            request: None,
            paused: true,
        });
        progress.readiness = Some(OpenReadiness::PausedPrepared);
        let receipt = verified_receipt(key(1), &enabled, &progress)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.readiness, OpenReadiness::PausedPrepared);
        assert_eq!(
            receipt.audio,
            AudioOutcome::Silent {
                source,
                reason: AudioSilence::Paused
            }
        );
        assert!(receipt.matches(&enabled));
        progress.readiness = None;
        assert!(
            verified_receipt(key(1), &enabled, &progress)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mismatched_owner_ack_blocks_cleanup_without_native_release() {
        let (mut runner, drivers) = runner(Config::default());
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        runner.stop(key(1).attempt, StopReason::Close);
        let endpoint = runner.endpoint.as_mut().unwrap();
        endpoint.wait_for_ack().unwrap();
        assert!(endpoint.rewrite_buffered_ack_generation(Generation::new(2).unwrap()));
        assert!(matches!(
            runner.poll(),
            Some(SessionEvent::OpenFailed { failure, .. })
                if failure.category == FailureCategory::Lifecycle(LifecycleFailure::Acknowledgement)
        ));
        assert!(matches!(
            runner.poll(),
            Some(SessionEvent::CleanupBlocked { .. })
        ));
        assert!(!runner.take_native_update().release_native);
        assert!(runner.endpoint.is_some());
        runner.native_released(key(1).attempt);
        assert_eq!(runner.attempt(), Some(key(1).attempt));
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn actual_audio_cancel_cleanup_error_keeps_quiescence_category() {
        let (mut runner, drivers) = runner(Config::default());
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        let failure = runner
            .map_failure(MediaError::new(
                "audio_cancel",
                "guard worker joined with cleanup error",
            ))
            .unwrap();
        assert_eq!(
            failure.category,
            FailureCategory::Lifecycle(LifecycleFailure::Quiescence)
        );
        assert_eq!(failure.operation, "audio_cancel");
        assert_eq!(failure.requested.as_ref(), &settings());
        runner.stop(key(1).attempt, StopReason::Close);
        stopped(&mut runner);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn ended_coalesced_with_ready_emits_terminal_before_readiness() {
        let (mut runner, drivers) = runner(Config {
            hold_shutdown: true,
            creates_handle: true,
            ..Config::default()
        });
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        runner.surface_ready(token(1));
        driver.initialized.recv().unwrap();
        let (load, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        driver.send(BackendEvent::PlaybackRestart);
        driver.send(BackendEvent::EndFile {
            reason: 0,
            error: 0,
        });
        driver.fence();

        assert_eq!(
            runner.poll(),
            Some(SessionEvent::StreamEnded {
                attempt: key(1).attempt,
                reason: 0,
                error: 0,
            })
        );
        assert!(runner.poll().is_none());
        assert!(!runner.verified);
        assert_eq!(runner.phase(), GatePhase::Opening);
        assert!(!runner.take_native_update().release_native);
        assert!(
            runner
                .endpoint
                .as_mut()
                .unwrap()
                .take_stopped()
                .unwrap()
                .is_none()
        );
        assert!(driver.shutdown_started.try_recv().is_err());

        assert_eq!(
            runner.stop(key(1).attempt, StopReason::Failed),
            StopSubmission::Accepted
        );
        driver.shutdown_started.recv().unwrap();
        assert!(runner.poll().is_none());
        assert!(!runner.take_native_update().release_native);
        driver.shutdown_release.send(()).unwrap();
        stopped(&mut runner);
        assert!(driver.destroyed.recv().unwrap());
        assert!(runner.take_native_update().release_native);
    }

    #[test]
    fn ended_coalesced_with_ready_fails_open_instead_of_committing() {
        use crate::domain::state::{CleanupStatus, ProductPhase};

        let requested = settings();
        let (runner, drivers) = runner(Config {
            hold_shutdown: true,
            creates_handle: true,
            ..Config::default()
        });
        let mut app = FixtureApp::new(
            requested.clone(),
            PlaybackGain::default(),
            FixtureValidator::default(),
            runner,
        );
        app.apply(app.model().state_identity(), app.model().draft().revision)
            .unwrap();
        app.poll();
        let (opening, _) = app.model().opening().unwrap();
        assert_eq!(app.model().phase(), ProductPhase::OpeningCandidate);
        assert!(app.runner_mut().take_native_update().create_native);
        let driver = drivers.recv().unwrap();
        app.runner_mut().surface_ready(token(opening.attempt.get()));
        driver.initialized.recv().unwrap();
        let (load, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        driver.send(BackendEvent::PlaybackRestart);
        driver.send(BackendEvent::EndFile {
            reason: 0,
            error: 0,
        });
        driver.fence();

        // The real consumer turns StreamEnded into candidate failure and stop;
        // the terminal event itself is not an owner-destruction acknowledgement.
        app.poll();
        let failure = app.model().failures().unwrap().candidate.clone().unwrap();
        assert_eq!(failure.category, FailureCategory::Session);
        assert_eq!(failure.requested.as_ref(), &requested);
        assert_eq!(app.model().phase(), ProductPhase::CleaningFailedCandidate);
        assert_eq!(app.model().cleanup(), &CleanupStatus::Draining);
        assert!(app.model().active().is_none());
        assert!(app.model().last_valid().is_none());
        driver.shutdown_started.recv().unwrap();
        assert!(driver.destroyed.try_recv().is_err());
        app.poll();
        assert!(app.runner_mut().endpoint.is_some());
        assert!(!app.runner_mut().take_native_update().release_native);
        assert_eq!(app.model().phase(), ProductPhase::CleaningFailedCandidate);

        driver.shutdown_release.send(()).unwrap();
        app.runner_mut()
            .endpoint
            .as_mut()
            .unwrap()
            .wait_for_ack()
            .unwrap();
        assert!(driver.destroyed.recv().unwrap());
        app.poll();
        assert!(app.runner_mut().endpoint.is_none());
        let release = app.runner_mut().take_native_update();
        assert!(release.release_native);
        assert_eq!(release.attempt, Some(opening.attempt));
        assert_eq!(app.model().phase(), ProductPhase::CleaningFailedCandidate);
        app.runner_mut().native_released(opening.attempt);
        app.poll();
        assert_eq!(app.model().phase(), ProductPhase::ErrorWithoutActive);
        assert_eq!(app.model().cleanup(), &CleanupStatus::Complete);
        assert!(app.model().active().is_none());
        assert!(app.model().last_valid().is_none());
        assert_eq!(
            app.model().failures().unwrap().candidate.as_ref(),
            Some(&failure)
        );
        assert!(drivers.try_recv().is_err());
    }
    fn fixture_app_live(config: Config) -> (FixtureApp, Driver, mpsc::Receiver<Driver>) {
        let (gate, drivers) = runner(config);
        let mut app = FixtureApp::new(
            settings(),
            PlaybackGain::default(),
            FixtureValidator::default(),
            gate,
        );
        app.apply(app.model().state_identity(), app.model().draft().revision)
            .unwrap();
        app.poll();
        let opening = app.model().opening().unwrap().0;
        let driver = drivers
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        fixture_app_ready(&mut app, &driver, opening.attempt);
        app.take_verified_applied()
            .expect("initial admitted LIVE Open receipt");
        (app, driver, drivers)
    }
    fn fixture_app_ready(app: &mut FixtureApp, driver: &Driver, attempt: AttemptId) {
        assert!(app.runner_mut().take_native_update().create_native);
        app.runner_mut().surface_ready(token(attempt.get()));
        driver
            .initialized
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (load, _) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        driver.send(BackendEvent::PlaybackRestart);
        driver.confirm_open_filters();
        driver.fence();
        fixture_wait(app, "scripted full filter readiness", |app| {
            app.model()
                .active()
                .is_some_and(|active| active.attempt() == attempt)
        });
    }
    fn fixture_chain(label: &str) -> crate::domain::filters::FilterChain {
        use crate::domain::filters::{
            ColorLevels, Filter, FilterChain, FilterEntry, FormatParams, SdrGamma, SdrMatrix,
        };
        FilterChain::new(vec![FilterEntry::new(
            label.into(),
            Filter::Format(FormatParams::new(
                SdrMatrix::Auto,
                ColorLevels::Auto,
                SdrGamma::Auto,
            )),
            true,
        )])
        .unwrap()
    }
    fn fixture_filter_request(
        app: &mut FixtureApp,
        driver: &Driver,
        chain: crate::domain::filters::FilterChain,
    ) -> (
        super::super::controller::RequestId,
        FilterAttemptKey,
        DraftSettings,
    ) {
        let mut candidate = app.model().draft().settings.clone();
        candidate.filters = chain;
        let revision = app
            .edit_draft(app.model().draft().revision, candidate.clone())
            .unwrap();
        let crate::domain::state::ApplyAdmission::Filters { key, .. } =
            app.apply(app.model().state_identity(), revision).unwrap()
        else {
            panic!("scripted same-source LIVE filter admission")
        };
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            matches!(command, super::super::controller::BackendCommand::ApplyFilters { key: actual, .. } if actual == key)
        );
        (id, key, candidate)
    }
    fn fixture_app_native_release(app: &mut FixtureApp, attempt: AttemptId) {
        fixture_wait(app, "actual owner destruction acknowledgement", |app| {
            app.runner_mut().endpoint.is_none()
        });
        let update = app.runner_mut().take_native_update();
        assert!(update.release_native);
        assert_eq!(update.attempt, Some(attempt));
        app.runner_mut().native_released(attempt);
        app.poll();
    }
    fn fixture_app_quit(app: &mut FixtureApp) {
        app.quit();
        fixture_wait(app, "scripted Quit retirement", |app| {
            let update = app.runner_mut().take_native_update();
            if update.release_native {
                app.runner_mut().native_released(update.attempt.unwrap());
            }
            app.model().shutdown_ready()
        });
    }
    #[test]
    fn gate_to_coordinator_retains_negative_with_ended_and_real_retirement_in_both_orders() {
        use crate::domain::state::{CleanupStatus, ProductPhase};
        for end_first in [false, true] {
            for retired_before_poll in [false, true] {
                for rejected in [false, true] {
                    let (mut app, driver, drivers) = fixture_app_live(Config {
                        hold_shutdown: true,
                        creates_handle: true,
                        ..Config::default()
                    });
                    let prior = app.model().last_valid().unwrap().settings().clone();
                    let old = app.model().active().unwrap().attempt();
                    let (id, key, candidate) = fixture_filter_request(
                        &mut app,
                        &driver,
                        fixture_chain("terminal candidate"),
                    );
                    let mut failure = filter_failure(key);
                    if rejected {
                        failure.kind = FilterErrorKind::CommandRejected { mpv_error: -5 };
                        failure.requires_fresh_owner = false;
                    }
                    let expected_kind = failure.kind.clone();
                    if end_first {
                        driver.send(BackendEvent::EndFile {
                            reason: 0,
                            error: 0,
                        });
                    }
                    driver.send(BackendEvent::FilterResult {
                        id: id.get(),
                        key,
                        result: Err(Box::new(failure)),
                    });
                    if !end_first {
                        driver.send(BackendEvent::EndFile {
                            reason: 0,
                            error: 0,
                        });
                    }
                    driver.fence();
                    if retired_before_poll {
                        app.runner_mut()
                            .endpoint
                            .as_ref()
                            .unwrap()
                            .stop(Generation::new(old.get()).unwrap(), None);
                        driver
                            .shutdown_started
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                        driver.shutdown_release.send(()).unwrap();
                        app.runner_mut()
                            .endpoint
                            .as_mut()
                            .unwrap()
                            .wait_for_ack()
                            .unwrap();
                        assert!(
                            driver
                                .destroyed
                                .recv_timeout(std::time::Duration::from_secs(5))
                                .unwrap()
                        );
                    }
                    app.poll();
                    assert_eq!(app.model().phase(), ProductPhase::RestoringFilters);
                    assert_eq!(
                        app.model().filtering().unwrap().route(),
                        Some(crate::domain::state::FilterRestoreRoute::FreshOwner)
                    );
                    assert!(
                        app.model().recovery().is_none(),
                        "filter terminal is not physical-removal recovery"
                    );
                    let cause = app.model().failures().unwrap().candidate.clone().unwrap();
                    assert_eq!(cause.requested.as_ref(), &candidate);
                    assert_eq!(cause.filter.as_ref().unwrap().kind, expected_kind);
                    assert_eq!(cause.filter.as_ref().unwrap().diagnostics.key, Some(key));
                    assert_eq!(app.model().last_valid().unwrap().settings(), &prior);
                    assert!(app.model().validation_request().is_none());
                    assert_eq!(app.model().cleanup(), &CleanupStatus::Draining);
                    if !retired_before_poll {
                        driver
                            .shutdown_started
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                        assert!(driver.destroyed.try_recv().is_err());
                        driver.shutdown_release.send(()).unwrap();
                        app.runner_mut()
                            .endpoint
                            .as_mut()
                            .unwrap()
                            .wait_for_ack()
                            .unwrap();
                        assert!(
                            driver
                                .destroyed
                                .recv_timeout(std::time::Duration::from_secs(5))
                                .unwrap()
                        );
                    }
                    fixture_app_native_release(&mut app, old);
                    let restored = app.model().opening().unwrap().0;
                    assert_eq!(restored.purpose, AttemptPurpose::Restore);
                    let restored_driver = drivers
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    fixture_app_ready(&mut app, &restored_driver, restored.attempt);
                    assert_eq!(app.model().phase(), ProductPhase::ErrorWithActiveRestored);
                    assert_eq!(app.model().active().unwrap().applied().settings(), &prior);
                    assert_eq!(app.model().draft().settings, candidate);
                    assert_eq!(
                        app.model().failures().unwrap().candidate.as_ref(),
                        Some(&cause)
                    );
                    assert_eq!(app.validator_mut().requests.len(), 2);
                    assert!(app.take_verified_applied().is_none());
                    assert!(!app.has_user_apply_result_or_pending(key.apply));
                    assert!(
                        drivers.try_recv().is_err(),
                        "one fresh owner, never a second route"
                    );
                    fixture_app_quit(&mut app);
                }
            }
        }
    }
    #[test]
    fn gate_to_coordinator_live_restore_ended_failure_never_opens_a_fresh_second_route() {
        use crate::domain::state::{CleanupStatus, ProductPhase};
        for end_first in [false, true] {
            let (mut app, driver, drivers) = fixture_app_live(Config {
                hold_shutdown: true,
                creates_handle: true,
                ..Config::default()
            });
            let old = app.model().active().unwrap().attempt();
            let prior = app.model().last_valid().unwrap().settings().clone();
            let (id, candidate, _) =
                fixture_filter_request(&mut app, &driver, fixture_chain("unconfirmed candidate"));
            let mut unconfirmed = filter_failure(candidate);
            unconfirmed.kind = FilterErrorKind::Unconfirmed {
                reason: crate::domain::failure::FilterConfirmationFailure::Deadline,
            };
            unconfirmed.requires_fresh_owner = false;
            driver.send(BackendEvent::FilterResult {
                id: id.get(),
                key: candidate,
                result: Err(Box::new(unconfirmed)),
            });
            driver.fence();
            app.poll();
            let restore = app.model().filtering().unwrap().key();
            assert_eq!(restore.pass, FilterPass::LiveRestore);
            let (id, command) = driver
                .submitted
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(
                matches!(command, super::super::controller::BackendCommand::ApplyFilters { key, .. } if key == restore)
            );
            if end_first {
                driver.send(BackendEvent::EndFile {
                    reason: 0,
                    error: 0,
                });
            }
            driver.send(BackendEvent::FilterResult {
                id: id.get(),
                key: restore,
                result: Err(Box::new(filter_failure(restore))),
            });
            if !end_first {
                driver.send(BackendEvent::EndFile {
                    reason: 0,
                    error: 0,
                });
            }
            driver.fence();
            app.poll();
            assert_eq!(app.model().phase(), ProductPhase::ErrorWithoutActive);
            assert!(app.model().failures().unwrap().candidate.is_some());
            let cause = app.model().failures().unwrap().restore.as_ref().unwrap();
            assert_eq!(cause.requested.as_ref(), &prior);
            assert_eq!(
                cause.filter.as_ref().unwrap().diagnostics.key,
                Some(restore)
            );
            driver
                .shutdown_started
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            driver.shutdown_release.send(()).unwrap();
            app.runner_mut()
                .endpoint
                .as_mut()
                .unwrap()
                .wait_for_ack()
                .unwrap();
            assert!(
                driver
                    .destroyed
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
            );
            fixture_app_native_release(&mut app, old);
            assert_eq!(app.model().cleanup(), &CleanupStatus::Complete);
            assert!(app.model().validation_request().is_none());
            assert!(app.model().recovery().is_none());
            assert_eq!(app.validator_mut().requests.len(), 1);
            assert!(
                drivers.try_recv().is_err(),
                "LiveRestore failure cannot transfer to a fresh route"
            );
            fixture_app_quit(&mut app);
        }
    }

    #[test]
    fn gate_to_coordinator_late_fault_plus_ended_restores_then_verified_chain_not_old_history() {
        use crate::domain::state::ProductPhase;
        for end_first in [false, true] {
            let (mut app, driver, drivers) = fixture_app_live(Config {
                hold_shutdown: true,
                creates_handle: true,
                ..Config::default()
            });
            let old = app.model().active().unwrap().attempt();
            let (id, key, applied) =
                fixture_filter_request(&mut app, &driver, fixture_chain("then verified chain"));
            driver.send(BackendEvent::FilterResult {
                id: id.get(),
                key,
                result: Ok(
                    crate::domain::state::FilterConfirmation::checked(key, 0.0, 2.0, 32).unwrap(),
                ),
            });
            driver.fence();
            fixture_wait(&mut app, "LIVE candidate confirmation", |app| {
                app.model().phase() == ProductPhase::Active
            });
            assert_eq!(app.model().last_valid().unwrap().settings(), &applied);
            if end_first {
                driver.send(BackendEvent::EndFile {
                    reason: 0,
                    error: 0,
                });
            }
            driver.send(BackendEvent::FilterFault {
                key,
                failure: Box::new(filter_failure(key)),
            });
            if !end_first {
                driver.send(BackendEvent::EndFile {
                    reason: 0,
                    error: 0,
                });
            }
            driver.fence();
            app.poll();
            assert_eq!(
                app.model().filtering().unwrap().prior().settings(),
                &applied
            );
            assert!(app.model().recovery().is_none());
            let cause = app.model().failures().unwrap().candidate.as_ref().unwrap();
            assert_eq!(cause.requested.as_ref(), &applied);
            assert_eq!(cause.filter.as_ref().unwrap().diagnostics.key, Some(key));
            driver
                .shutdown_started
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            driver.shutdown_release.send(()).unwrap();
            app.runner_mut()
                .endpoint
                .as_mut()
                .unwrap()
                .wait_for_ack()
                .unwrap();
            assert!(
                driver
                    .destroyed
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
            );
            fixture_app_native_release(&mut app, old);
            let restore = app.model().opening().unwrap().0;
            assert_eq!(restore.purpose, AttemptPurpose::Restore);
            let restored_driver = drivers
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            fixture_app_ready(&mut app, &restored_driver, restore.attempt);
            assert_eq!(app.model().phase(), ProductPhase::ErrorWithActiveRestored);
            assert_eq!(app.model().active().unwrap().applied().settings(), &applied);
            assert_eq!(app.validator_mut().requests.len(), 2);
            assert!(app.take_verified_applied().is_none());
            assert!(drivers.try_recv().is_err());
            fixture_app_quit(&mut app);
        }
    }
    #[test]
    fn gate_to_coordinator_watcher_removal_overrides_graph_and_ended_and_recovers_only_prior() {
        use crate::domain::capture::{
            LossEvidence, ObservationEpoch, RecoveryObservation, SourcePresence, VideoPresence,
        };
        let (mut app, driver, drivers) = fixture_app_live(Config {
            hold_shutdown: true,
            creates_handle: true,
            ..Config::default()
        });
        let old = app.model().active().unwrap().attempt();
        let prior = app.model().last_valid().unwrap().settings().clone();
        let (id, key, candidate) = fixture_filter_request(
            &mut app,
            &driver,
            fixture_chain("cancelled graph candidate"),
        );
        driver.send(BackendEvent::FilterResult {
            id: id.get(),
            key,
            result: Err(Box::new(filter_failure(key))),
        });
        driver.send(BackendEvent::EndFile {
            reason: 0,
            error: 0,
        });
        driver.fence();
        let stamp = WatchStamp {
            watch: app.model().watch_target().unwrap().watch,
            epoch: ObservationEpoch::new(2).unwrap(),
        };
        app.validator_mut().observation = Some(RecoveryObservation {
            stamp,
            video: VideoPresence::Present,
            audio: SourcePresence::Disabled,
            last_video_removal: Some(stamp.epoch),
        });
        app.poll();
        assert!(app.model().filtering().is_none());
        let loss = app.model().recovery().unwrap();
        assert_eq!(loss.applied.settings(), &prior);
        assert_eq!(loss.evidence, LossEvidence::Removed { stamp });
        assert_eq!(app.model().draft().settings, candidate);
        assert!(!app.has_user_apply_result_or_pending(key.apply));
        driver
            .shutdown_started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        driver.shutdown_release.send(()).unwrap();
        app.runner_mut()
            .endpoint
            .as_mut()
            .unwrap()
            .wait_for_ack()
            .unwrap();
        assert!(
            driver
                .destroyed
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        );
        fixture_app_native_release(&mut app, old);
        fixture_wait(&mut app, "ordinary source Recovery opening", |app| {
            app.model()
                .opening()
                .is_some_and(|(key, _)| key.purpose == AttemptPurpose::Recovery)
        });
        let recovery = app.model().opening().unwrap().0;
        assert_eq!(recovery.purpose, AttemptPurpose::Recovery);
        let recovered_driver = drivers
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        fixture_app_ready(&mut app, &recovered_driver, recovery.attempt);
        assert_eq!(app.model().active().unwrap().applied().settings(), &prior);
        assert_eq!(app.model().draft().settings, candidate);
        assert!(drivers.try_recv().is_err());
        fixture_app_quit(&mut app);
    }

    #[test]
    fn gate_to_coordinator_stopping_retains_copied_late_fault_and_supersedes_source_open() {
        use crate::domain::state::ProductPhase;
        let (mut app, driver, drivers) = fixture_app_live(Config {
            hold_shutdown: true,
            creates_handle: true,
            ..Config::default()
        });
        let old = app.model().active().unwrap().attempt();
        let prior = app.model().last_valid().unwrap().settings().clone();
        let confirmed = app.model().confirmed_filter_key().unwrap();
        let mut source = prior.clone();
        source.video.mode.rate = FrameRate::new(30, 1).unwrap();
        source.filters = fixture_chain("superseded source candidate");
        app.edit_draft(app.model().draft().revision, source)
            .unwrap();
        let source_admission = app
            .apply(app.model().state_identity(), app.model().draft().revision)
            .unwrap();
        app.poll();
        assert_eq!(app.model().phase(), ProductPhase::ClosingOld);
        driver
            .shutdown_started
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let mut newer = app.model().draft().settings.clone();
        newer.filters = fixture_chain("newer editable draft");
        app.edit_draft(app.model().draft().revision, newer.clone())
            .unwrap();
        // Emulate an already-copied sticky native fault delivered while shutdown
        // is blocked; the consumer still uses GateRunner::poll, not app events.
        app.runner_mut().endpoint.as_ref().unwrap().take_snapshot();
        let sequence = app
            .runner_mut()
            .last_filter
            .as_ref()
            .unwrap()
            .sequence
            .checked_add(1)
            .unwrap();
        let mut late = snapshot(&prior, AudioAvailability::Disabled);
        late.generation = Generation::new(old.get()).unwrap();
        late.filters = Some(FilterSnapshot {
            key: confirmed,
            sequence,
            status: FilterStatus::Failed(Box::new(filter_failure(confirmed))),
        });
        app.runner_mut().retained_progress = Some(late);
        app.poll();
        assert_eq!(app.model().phase(), ProductPhase::RestoringFilters);
        assert_eq!(app.model().filtering().unwrap().prior().settings(), &prior);
        assert!(!app.has_user_apply_result_or_pending(source_admission.id()));
        assert_eq!(app.model().draft().settings, newer);
        assert_eq!(
            app.model()
                .failures()
                .unwrap()
                .candidate
                .as_ref()
                .unwrap()
                .filter
                .as_ref()
                .unwrap()
                .diagnostics
                .key,
            Some(confirmed)
        );
        assert!(
            drivers.try_recv().is_err(),
            "superseded source candidate cannot open before or after the barrier"
        );
        assert!(driver.destroyed.try_recv().is_err());
        driver.shutdown_release.send(()).unwrap();
        app.runner_mut()
            .endpoint
            .as_mut()
            .unwrap()
            .wait_for_ack()
            .unwrap();
        assert!(
            driver
                .destroyed
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        );
        fixture_app_native_release(&mut app, old);
        let restore = app.model().opening().unwrap().0;
        assert_eq!(restore.purpose, AttemptPurpose::Restore);
        let restored_driver = drivers
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        fixture_app_ready(&mut app, &restored_driver, restore.attempt);
        assert_eq!(app.model().phase(), ProductPhase::ErrorWithActiveRestored);
        assert_eq!(app.model().active().unwrap().applied().settings(), &prior);
        assert_eq!(app.model().draft().settings, newer);
        assert_eq!(app.validator_mut().requests.len(), 3);
        assert!(app.take_verified_applied().is_none());
        assert!(
            drivers.try_recv().is_err(),
            "only the chosen fresh Restore may own the replacement"
        );
        fixture_app_quit(&mut app);
    }

    fn filter_failure(key: FilterAttemptKey) -> FilterFailure {
        FilterFailure {
            kind: FilterErrorKind::RuntimeGraph,
            attributed_ordinal: None,
            requires_fresh_owner: true,
            diagnostics: FilterAttemptDiagnostics {
                key: Some(key),
                entries: Vec::new(),
                records: Vec::new(),
                native_evidence_lost: false,
                truncated: false,
                dropped_context: 0,
            },
        }
    }

    #[test]
    fn filter_preparation_uses_cached_refusal_without_owner_io() {
        let mut gate = GateRunner::with_spawner(
            Err(FilterCatalogError::InvalidPrefix {
                detail: "observed startup refusal".into(),
            }),
            |_, _| panic!("prevalidation cannot create an owner"),
        );
        let before = gate.state.phase();
        let failure = gate.prepare_filters(&settings()).unwrap_err();
        assert_eq!(
            failure.category,
            FailureCategory::Validation(ValidationLayer::Filters)
        );
        assert_eq!(failure.stage, Stage::Prevalidation);
        let filter = failure.filter.unwrap();
        assert_eq!(filter.kind, FilterErrorKind::CatalogUnavailable);
        assert_eq!(
            filter.diagnostics.key, None,
            "no invented initial physical owner"
        );
        assert_eq!(gate.state.phase(), before);
        assert!(gate.endpoint.is_none() && gate.key.is_none());
        assert!(!gate.take_native_update().create_native);
    }
    #[test]
    fn complete_treatment_preparation_keeps_original_ordinals_and_disabled_metadata_without_owner_io()
     {
        let mut gate = GateRunner::with_spawner(
            Ok(crate::media::filter_catalog::fixture_capabilities()),
            |_, _| panic!("in-memory preparation cannot create an owner"),
        );
        let mut requested = settings();
        requested.filters = native_chain();
        let compiled = gate.prepare_filters(&requested).unwrap();
        assert!(compiled.vf.starts_with("@furami_0:"));
        assert!(compiled.vf.contains(",@furami_2:"));
        assert!(!compiled.vf.contains("@furami_1:"));
        assert!(!compiled.vf.contains("disabled eq"));
        assert_eq!(compiled.entries.len(), 3);
        assert_eq!(compiled.entries[1].ordinal, 1);
        assert_eq!(compiled.entries[1].label, "disabled eq\ninert");
        assert!(!compiled.entries[1].enabled);
        assert!(gate.endpoint.is_none() && gate.key.is_none());
        assert!(!gate.take_native_update().create_native);
    }

    #[test]
    fn live_open_requires_exact_open_filter_proof_and_paused_preparation_is_not_live() {
        let settings = settings();
        let mut progress = snapshot(&settings, AudioAvailability::Disabled);
        progress.filters = None;
        assert!(
            verified_receipt(key(1), &settings, &progress)
                .unwrap()
                .is_none()
        );
        let open = FilterAttemptKey {
            apply: key(1).apply,
            attempt: key(1).attempt,
            pass: FilterPass::Open,
        };
        progress.filters = Some(FilterSnapshot {
            key: open,
            sequence: 1,
            status: FilterStatus::Pending,
        });
        assert!(
            verified_receipt(key(1), &settings, &progress)
                .unwrap()
                .is_none()
        );
        let wrong = FilterAttemptKey {
            pass: FilterPass::LiveCandidate,
            ..open
        };
        progress.filters = Some(FilterSnapshot {
            key: wrong,
            sequence: 2,
            status: FilterStatus::Confirmed(
                crate::domain::state::FilterConfirmation::checked(wrong, 1.0, 3.0, 32).unwrap(),
            ),
        });
        assert!(
            verified_receipt(key(1), &settings, &progress)
                .unwrap()
                .is_none()
        );
        progress.filters.as_mut().unwrap().key = open;
        assert!(
            !filter_open_ready(key(1), &progress),
            "envelope alone cannot authorize mismatched confirmation"
        );
        assert!(
            verified_receipt(key(1), &settings, &progress)
                .unwrap()
                .is_none()
        );
        progress.readiness = Some(OpenReadiness::PausedPrepared);
        progress.playback = InitialPlayback::Paused;
        progress.filters = None;
        let receipt = verified_receipt(key(1), &settings, &progress)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.filters, FilterOpenReceipt::PreparedPaused);
        assert!(receipt.matches_filters(key(1)));
        progress.playback = InitialPlayback::Live;
        assert!(
            verified_receipt(key(1), &settings, &progress)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn sticky_filter_fault_overwrites_unconsumed_confirmation_and_deduplicates_status() {
        let (mut gate, drivers) = runner(Config::default());
        open(&mut gate, 1);
        let driver = drivers.recv().unwrap();
        let filter_key = FilterAttemptKey {
            apply: key(1).apply,
            attempt: key(1).attempt,
            pass: FilterPass::LiveCandidate,
        };
        let mut progress = snapshot(&settings(), AudioAvailability::Disabled);
        progress.filters = Some(FilterSnapshot {
            key: filter_key,
            sequence: 1,
            status: FilterStatus::Confirmed(
                crate::domain::state::FilterConfirmation::checked(filter_key, 0.0, 2.0, 32)
                    .unwrap(),
            ),
        });
        gate.project_filters(&progress);
        assert!(matches!(
            gate.filter_event,
            Some(SessionEvent::FilterResult { result: Ok(_), .. })
        ));
        progress.filters.as_mut().unwrap().status =
            FilterStatus::Failed(Box::new(filter_failure(filter_key)));
        gate.project_filters(&progress);
        assert!(
            matches!(gate.filter_event.take(), Some(SessionEvent::FilterFault { key, .. }) if key == filter_key)
        );
        gate.project_filters(&progress);
        assert!(
            gate.filter_event.is_none(),
            "same sequence/status must not repeat"
        );
        gate.stop(key(1).attempt, StopReason::Close);
        driver.destroyed.recv().unwrap();
        stopped(&mut gate);
        gate.native_released(key(1).attempt);
        gate.poll();
    }

    #[test]
    fn terminal_owner_snapshot_cannot_publish_filter_confirmation() {
        let (mut gate, drivers) = runner(Config::default());
        open(&mut gate, 1);
        let driver = drivers.recv().unwrap();
        let mut progress = snapshot(&settings(), AudioAvailability::Disabled);
        progress.failure = Some(MediaError::new(
            "terminal_fixture",
            "physical owner failure",
        ));
        assert!(matches!(
            gate.snapshot(progress),
            Some(SessionEvent::OpenFailed { .. })
        ));
        assert!(gate.filter_event.is_none());
        assert!(!gate.verified);
        driver.destroyed.recv().unwrap();
        stopped(&mut gate);
        gate.native_released(key(1).attempt);
        gate.poll();
    }
    #[test]
    fn terminal_snapshot_retains_typed_filter_cause_instead_of_flattening_it() {
        let (mut gate, drivers) = runner(Config::default());
        open(&mut gate, 1);
        let driver = drivers.recv().unwrap();
        let open_key = FilterAttemptKey {
            apply: key(1).apply,
            attempt: key(1).attempt,
            pass: FilterPass::Open,
        };
        let mut progress = snapshot(&settings(), AudioAvailability::Disabled);
        progress.filters = Some(FilterSnapshot {
            key: open_key,
            sequence: 2,
            status: FilterStatus::Failed(Box::new(filter_failure(open_key))),
        });
        progress.failure = Some(MediaError::new(
            "terminal_fixture",
            "physical owner failure",
        ));
        let Some(SessionEvent::OpenFailed { failure, .. }) = gate.snapshot(progress) else {
            panic!("terminal filter failure must revoke opening");
        };
        assert_eq!(failure.filter.unwrap().kind, FilterErrorKind::RuntimeGraph);
        gate.endpoint.as_mut().unwrap().wait_for_ack().unwrap();
        // Verify port-visible outcomes, not the gate's private retained slot.
        let mut retained_negative = false;
        let mut owner_stopped = false;
        while let Some(event) = gate.poll() {
            match event {
                SessionEvent::FilterResult {
                    key,
                    result: Err(failure),
                }
                | SessionEvent::FilterFault { key, failure } => {
                    assert_eq!(key, open_key);
                    assert_eq!(failure.kind, FilterErrorKind::RuntimeGraph);
                    retained_negative = true;
                }
                SessionEvent::OpenVerified { .. }
                | SessionEvent::FilterResult { result: Ok(_), .. } => {
                    panic!("a terminal owner cannot publish a successful open/filter receipt");
                }
                SessionEvent::OwnerStopped { attempt, .. } => {
                    assert_eq!(attempt, key(1).attempt);
                    owner_stopped = true;
                }
                _ => {}
            }
        }
        assert!(
            retained_negative,
            "typed negative remains observable after terminal opening failure"
        );
        driver.destroyed.recv().unwrap();
        assert!(
            owner_stopped,
            "actual owner retirement remains observable before native release"
        );
        gate.native_released(key(1).attempt);
        assert!(
            matches!(gate.poll(), Some(SessionEvent::NativeReleased { attempt }) if attempt == key(1).attempt)
        );
    }

    fn assert_native_confirmations(evidence: &[serde_json::Value]) {
        for (result_index, result) in evidence
            .iter()
            .enumerate()
            .filter(|(_, event)| event["event"] == "result" && event["result"].get("Ok").is_some())
        {
            let confirmation = &result["result"]["Ok"];
            let key = &result["key"];
            let command_index = evidence[..result_index]
                .iter()
                .rposition(|event| {
                    event["event"] == "command"
                        && &event["key"] == key
                        && event["request"] == result["request"]
                })
                .expect("confirmation must retain exact command");
            let admission_index = evidence[..command_index]
                .iter()
                .rposition(|event| {
                    event["event"] == "admission"
                        && &event["key"] == key
                        && event["request"] == result["request"]
                })
                .expect("native pass must retain exact keyed admission");
            assert!(
                evidence[admission_index + 1..command_index]
                    .iter()
                    .any(|event| {
                        event["event"] == "drain"
                            && event["stage"] == "pre-submit"
                            && &event["key"] == key
                            && event["request"] == result["request"]
                    }),
                "keyed admission must precede clean raw MPV_EVENT_NONE drain, then command submission"
            );
            let reply_index = evidence[command_index..result_index]
                .iter()
                .position(|event| {
                    event["event"] == "reply"
                        && &event["key"] == key
                        && event["request"] == result["request"]
                        && event["error"].as_i64().is_some_and(|error| error >= 0)
                })
                .map(|index| command_index + index)
                .expect("matching successful native command reply");
            let reconfig_index = evidence[command_index..result_index]
                .iter()
                .position(|event| event["event"] == "reconfig" && &event["key"] == key)
                .map(|index| command_index + index)
                .expect("in-window native reconfiguration");
            let baseline = confirmation["baseline"]
                .as_f64()
                .expect("finite confirmed baseline");
            let last = confirmation["last_position"]
                .as_f64()
                .expect("finite final sample");
            let mut prior = baseline;
            let mut advances = 0;
            let mut saw_baseline = false;
            let progress_start = reply_index.max(reconfig_index) + 1;
            let mut final_sample_index = None;
            for (offset, sample) in evidence[progress_start..result_index]
                .iter()
                .enumerate()
                .filter(|(_, event)| event["event"] == "progress" && &event["key"] == key)
            {
                assert!(sample["error"].as_i64().is_some_and(|error| error >= 0));
                let position = sample["position"].as_f64().expect("numeric native sample");
                assert!(position.is_finite() && position >= prior);
                if !saw_baseline {
                    assert_eq!(position, baseline);
                    saw_baseline = true;
                } else if position > prior {
                    advances += 1;
                }
                prior = position;
                final_sample_index = Some(progress_start + offset);
            }
            assert!(saw_baseline);
            assert_eq!(
                advances, 32,
                "exact 32 post-baseline strict native advances"
            );
            assert_eq!(prior, last);
            assert_eq!(confirmation["advances"], 32);
            let final_sample_index =
                final_sample_index.expect("confirmed final native progress reply");
            assert!(
                evidence[final_sample_index + 1..result_index]
                    .iter()
                    .any(|event| {
                        event["event"] == "drain"
                            && event["stage"] == "post-progress"
                            && &event["key"] == key
                            && event["request"] == result["request"]
                    }),
                "consumed final sample must precede clean raw MPV_EVENT_NONE drain, then success publication"
            );
        }
    }

    fn native_filter_runner(
        prefix: String,
        fixture: std::path::PathBuf,
        recorder: crate::media::ffi::FixtureRecorder,
        fault: Option<(crate::media::ffi::FixtureFault, FilterPass)>,
        restore_fault: bool,
    ) -> GateRunner {
        let capabilities = super::super::filter_catalog::query_qualified_capabilities(
            std::path::Path::new(&prefix),
        )
        .expect("genuine qualified capabilities required by native contract");
        let mut empty_opens = 0;
        GateRunner::with_spawner(Ok(capabilities), move |generation, config| {
            let input = config
                .video
                .validate_snapshot(&session_fixture(
                    &["/dev/video0"],
                    config.video.requested().mode,
                ))
                .map_err(|error| MediaError::new("fixture_input", error.to_string()))?;
            let requested = input.requested().clone();
            if config.filters.entries().is_empty() {
                empty_opens += 1;
            }
            let inject_restore =
                restore_fault && config.filters.entries().is_empty() && empty_opens > 1;
            let prefix = prefix.clone();
            let fixture = fixture.clone();
            let recorder = recorder.clone();
            OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || {
                let backend = crate::media::ffi::FixtureBackend::new_with_filters(
                    prefix,
                    fixture,
                    input,
                    config.gain,
                    config.playback,
                    config.watch,
                    config.filters,
                    config.compiled_filters,
                    config.filter_key,
                    Some(recorder),
                )
                .expect("validated native fixture backend");
                if inject_restore {
                    backend.with_fault_for_pass(
                        crate::media::ffi::FixtureFault::RuntimeGraph,
                        FilterPass::Open,
                    )
                } else if let Some((fault, pass)) = fault {
                    backend.with_fault_for_pass(fault, pass)
                } else {
                    backend
                }
            })
        })
    }

    fn native_gate_event<T>(
        gate: &mut GateRunner,
        label: &str,
        mut accept: impl FnMut(SessionEvent) -> Option<T>,
    ) -> T {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Some(event) = gate.poll() {
                eprintln!("native gate {label}: {event:?}");
                if let Some(result) = accept(event) {
                    return result;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native {label} timeout: phase={:?}, failure={:?}",
                gate.phase(),
                gate.state.failure()
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn native_gate_retire(gate: &mut GateRunner) {
        let Some(attempt) = gate.attempt() else {
            return;
        };
        gate.stop(attempt, StopReason::Quit);
        if gate.endpoint.is_some() {
            native_gate_event(gate, "genuine owner destruction", |event| match event {
                SessionEvent::OwnerStopped {
                    attempt: stopped,
                    outcome,
                } if stopped == attempt => {
                    outcome.expect("native fixture owner must retire cleanly");
                    Some(())
                }
                _ => None,
            });
        }
        assert!(gate.endpoint.is_none());
        assert_eq!(gate.phase(), GatePhase::Releasing);
        gate.native_released(attempt);
        native_gate_event(gate, "native release", |event| match event {
            SessionEvent::NativeReleased { attempt: released } if released == attempt => Some(()),
            _ => None,
        });
        assert!(gate.key.is_none());
    }

    fn native_app_wait(
        app: &mut FixtureApp,
        label: &str,
        mut ready: impl FnMut(&FixtureApp) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(35);
        loop {
            app.poll();
            let update = app.runner_mut().take_native_update();
            if update.create_native {
                app.runner_mut()
                    .surface_ready(token(update.attempt.unwrap().get()));
            }
            if update.release_native {
                assert!(
                    app.runner_mut().endpoint.is_none(),
                    "no fake destruction acknowledgement"
                );
                assert!(
                    app.runner_mut().owner_event.is_none(),
                    "coordinator must consume real owner result"
                );
                app.runner_mut().native_released(update.attempt.unwrap());
            }
            if ready(app) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native app {label} timeout: state={:?}, failures={:?}",
                app.model().state_identity(),
                app.model().failures()
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn native_chain() -> crate::domain::filters::FilterChain {
        use crate::domain::filters::*;
        FilterChain::new(vec![
            FilterEntry::new(
                "format reference".into(),
                Filter::Format(FormatParams::new(
                    SdrMatrix::Auto,
                    ColorLevels::Auto,
                    SdrGamma::Auto,
                )),
                true,
            ),
            FilterEntry::new(
                "disabled eq\ninert".into(),
                Filter::Eq(
                    EqParams::new(EqValues {
                        contrast: 1.0,
                        brightness: 0.0,
                        saturation: 1.0,
                        gamma: 1.0,
                        gamma_r: 1.0,
                        gamma_g: 1.0,
                        gamma_b: 1.0,
                        gamma_weight: 1.0,
                    })
                    .unwrap(),
                ),
                false,
            ),
            FilterEntry::new(
                "retained hqdn3d".into(),
                Filter::Hqdn3d(
                    Hqdn3dParams::new(Hqdn3dValues {
                        luma_spatial: 0.0,
                        chroma_spatial: 0.0,
                        luma_tmp: 0.0,
                        chroma_tmp: 0.0,
                    })
                    .unwrap(),
                ),
                true,
            ),
        ])
        .unwrap()
    }

    fn native_submit_chain(
        gate: &mut GateRunner,
        apply: u64,
        chain: crate::domain::filters::FilterChain,
    ) -> Result<crate::domain::state::FilterConfirmation, Box<FilterFailure>> {
        let key = FilterAttemptKey {
            apply: ApplyId::new(apply).unwrap(),
            attempt: gate.attempt().unwrap(),
            pass: FilterPass::LiveCandidate,
        };
        let mut settings = gate.requested.clone().unwrap();
        settings.filters = chain;
        let compiled = gate
            .prepare_filters(&settings)
            .expect("native chain must compile");
        eprintln!(
            "native submitted chain: {}",
            serde_json::json!({"key": key, "chain": settings.filters, "vf": compiled.vf})
        );
        assert_eq!(gate.submit_filters(key, compiled), SubmitStatus::Accepted);
        native_gate_event(gate, "whole chain result", |event| match event {
            SessionEvent::FilterResult {
                key: observed,
                result,
            } if observed == key => Some(result),
            SessionEvent::FilterFault {
                key: observed,
                failure,
            } if observed == key => Some(Err(failure)),
            SessionEvent::OpenFailed { failure, .. }
            | SessionEvent::SessionFailed { failure, .. } => {
                panic!("unexpected whole-owner failure for chain: {failure:?}")
            }
            _ => None,
        })
    }

    #[test]
    #[ignore = "requires frozen libmpv and absolute normal/tiny FURAMI filter fixtures"]
    fn real_libmpv_filter_transactions() {
        use crate::domain::filters::*;
        use crate::domain::state::ProductPhase;
        use crate::media::ffi::{FixtureFault, FixtureMilestone, FixtureRecorder};
        use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
        let _ = tracing_subscriber::fmt()
            .with_env_filter("furami=trace")
            .with_test_writer()
            .try_init();
        let prefix = std::env::var("FURAMI_MEDIA_PREFIX")
            .expect("FURAMI_MEDIA_PREFIX required; no silent native skip");
        let prefix_path = std::path::Path::new(&prefix);
        assert!(
            prefix_path.is_absolute() && prefix_path.is_dir(),
            "absolute frozen media prefix required"
        );
        let fixture = |name| {
            let path = std::path::PathBuf::from(
                std::env::var_os(name)
                    .unwrap_or_else(|| panic!("{name} required; no silent native skip")),
            );
            assert!(
                path.is_absolute() && path.is_file(),
                "absolute existing {name} fixture required"
            );
            path.canonicalize()
                .expect("native fixture canonicalization")
        };
        let normal = fixture("FURAMI_PLAYBACK_FIXTURE");
        let tiny = fixture("FURAMI_FILTER_FAILURE_FIXTURE");
        assert_ne!(
            normal, tiny,
            "normal and 4x4 failing fixture must be distinct"
        );
        let library = prefix_path
            .join("lib/libmpv.so")
            .canonicalize()
            .expect("frozen libmpv artifact required");
        assert!(
            library.starts_with(prefix_path.canonicalize().unwrap()),
            "libmpv must remain inside frozen prefix"
        );
        eprintln!(
            "native contract artifacts: {}",
            serde_json::json!({
                "prefix": prefix, "libmpv": library, "normal": normal, "tiny": tiny,
                "sample_ms": 50, "strict_advances": 32, "pass_deadline_ms": 10000,
            })
        );

        let recorder = FixtureRecorder::default();
        let mut gate = native_filter_runner(
            prefix.clone(),
            normal.clone(),
            recorder.clone(),
            None,
            false,
        );
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut requested = settings();
            requested.video.mode.size = FrameSize::new(320, 240).unwrap();
            requested.video.mode.rate = FrameRate::new(30, 1).unwrap();
            let prepared = fixture_prepared(requested.clone()).unwrap();
            let compiled = gate.prepare_filters(&requested).unwrap();
            gate.begin_open(
                key(1),
                prepared,
                compiled,
                PlaybackGain::default(),
                InitialPlayback::Live,
                output_fixture_plan(),
            )
            .unwrap();
            assert!(gate.take_native_update().create_native);
            gate.surface_ready(token(1));
            let receipt =
                native_gate_event(&mut gate, "native initial empty confirmation", |event| {
                    match event {
                        SessionEvent::OpenVerified { receipt, .. } => Some(receipt),
                        SessionEvent::OpenFailed { failure, .. } => {
                            panic!("initial contract contradiction: {failure:?}")
                        }
                        _ => None,
                    }
                });
            assert!(receipt.matches(&requested) && receipt.matches_filters(key(1)));
            let confirmed = native_submit_chain(&mut gate, 2, FilterChain::default())
                .expect("vf set empty clear must satisfy exact witness");
            assert_eq!(confirmed.advances(), 32);
            let chain = native_chain();
            native_submit_chain(&mut gate, 3, chain.clone())
                .expect("three entry/middle-disabled contract");
            let mut replacement = chain.clone();
            replacement
                .replace(
                    0,
                    FilterEntry::new(
                        "format reference".into(),
                        Filter::Format(FormatParams::new(
                            SdrMatrix::Bt709,
                            ColorLevels::Limited,
                            SdrGamma::Bt1886,
                        )),
                        true,
                    ),
                )
                .unwrap();
            native_submit_chain(&mut gate, 4, replacement.clone())
                .expect("retained-entry replacement must independently confirm");
            replacement.move_entry(2, 0).unwrap();
            native_submit_chain(&mut gate, 5, replacement.clone())
                .expect("retained-entry reorder must independently confirm");
            native_submit_chain(&mut gate, 6, replacement.clone())
                .expect("identical-chain replay must independently confirm");
            replacement.set_enabled(0, false).unwrap();
            replacement.set_enabled(1, false).unwrap();
            native_submit_chain(&mut gate, 7, replacement)
                .expect("all-disabled chain clears via same command");
            native_submit_chain(&mut gate, 8, FilterChain::default())
                .expect("final empty chain clear");
            let loaded = std::fs::read_to_string("/proc/self/maps").expect("loaded library paths");
            for line in loaded.lines().filter(|line| {
                line.contains("libmpv")
                    || line.contains("libavfilter")
                    || line.contains("libplacebo")
            }) {
                eprintln!("native loaded artifact: {line}");
            }
        }));
        native_gate_retire(&mut gate);
        let evidence = recorder.native_snapshot().unwrap();
        eprintln!(
            "native normal raw evidence: {}",
            serde_json::to_string(&evidence).unwrap()
        );
        if let Err(payload) = result {
            resume_unwind(payload)
        }
        assert_native_confirmations(&evidence);
        let commands: Vec<_> = evidence
            .iter()
            .filter(|event| event["event"] == "command")
            .collect();
        assert_eq!(
            commands.len(),
            8,
            "one whole-chain command for initial Open and each of seven requests"
        );
        for command in &commands {
            assert_eq!(command["argv"][0], "vf");
            assert_eq!(command["argv"][1], "set");
            assert_eq!(command["argv"].as_array().unwrap().len(), 3);
        }
        let three = commands
            .iter()
            .find(|event| event["key"]["apply"] == 3)
            .unwrap()["argv"][2]
            .as_str()
            .unwrap();
        assert!(three.starts_with("@furami_0:") && three.contains(",@furami_2:"));
        assert!(!three.contains("@furami_1:") && !three.contains("disabled eq"));
        for apply in [1, 2, 7, 8] {
            assert_eq!(
                commands
                    .iter()
                    .find(|event| event["key"]["apply"] == apply)
                    .unwrap()["argv"][2],
                ""
            );
        }
        // Complete treatments persist through real pause, preparation-only
        // paused reconnect, and fresh LIVE resume/restart owners.
        let recorder = FixtureRecorder::default();
        let gate = native_filter_runner(
            prefix.clone(),
            normal.clone(),
            recorder.clone(),
            None,
            false,
        );
        let mut prior = settings();
        prior.video.mode.size = FrameSize::new(320, 240).unwrap();
        prior.video.mode.rate = FrameRate::new(30, 1).unwrap();
        prior.filters = native_chain();
        let mut app = FixtureApp::new(
            prior.clone(),
            PlaybackGain::default(),
            FixtureValidator::default(),
            gate,
        );
        let result = catch_unwind(AssertUnwindSafe(|| {
            app.set_output_plan(output_fixture_plan()).unwrap();
            let admission = app
                .apply(app.model().state_identity(), app.model().draft().revision)
                .unwrap();
            native_app_wait(&mut app, "complete startup LIVE filter proof", |app| {
                app.model().active().is_some()
            });
            let first = app.model().active().unwrap().attempt();
            assert!(
                matches!(app.take_verified_applied(), Some(crate::app::apply::VerifiedApplied::Open { key, applied })
                if key.apply == admission.id() && applied.settings() == &prior)
            );
            assert_eq!(app.pause(first).unwrap(), SubmitStatus::Accepted);
            native_app_wait(&mut app, "actual pause readback", |app| {
                app.model().active().is_some_and(|active| {
                    active.playback() == crate::domain::state::PlaybackState::Paused
                })
            });
            let before_pause_close = recorder.native_snapshot().unwrap();
            assert_eq!(
                before_pause_close
                    .iter()
                    .filter(|event| event["event"] == "command")
                    .count(),
                1,
                "ordinary pause must issue no vf command"
            );
            let mut newer = prior.clone();
            newer.filters = FilterChain::default();
            app.edit_draft(app.model().draft().revision, newer.clone())
                .unwrap();
            app.close(app.model().state_identity()).unwrap();
            native_app_wait(&mut app, "real paused close barrier", |app| {
                app.model().phase() == ProductPhase::Stopped
                    && app.model().cleanup() == &crate::domain::state::CleanupStatus::Complete
            });
            assert!(matches!(
                app.reconnect(app.model().state_identity()).unwrap(),
                crate::domain::state::ReconnectAdmission::Started(_)
            ));
            native_app_wait(&mut app, "complete paused reconnect preparation", |app| {
                app.model().active().is_some()
            });
            let prepared = app.model().active().unwrap();
            assert_eq!(
                prepared.playback(),
                crate::domain::state::PlaybackState::Paused
            );
            assert_eq!(prepared.applied().settings(), &prior);
            let paused_owner = prepared.attempt();
            assert_ne!(paused_owner, first);
            assert!(
                app.take_verified_applied().is_none(),
                "PreparedPaused never authorizes save"
            );
            let after_preparation = recorder.native_snapshot().unwrap();
            assert_eq!(
                after_preparation
                    .iter()
                    .filter(|event| event["event"] == "command")
                    .count(),
                1,
                "paused preparation must not temporarily unpause or submit vf set"
            );
            let option = after_preparation
                .iter()
                .find(|event| event["event"] == "paused_filter_option")
                .expect("real checked vf option before paused load");
            let compiled = app.runner_mut().prepare_filters(&prior).unwrap();
            assert_eq!(option["vf"], compiled.vf);
            app.resume(app.model().state_identity(), paused_owner)
                .unwrap();
            native_app_wait(&mut app, "fresh complete LIVE resume proof", |app| {
                app.model().active().is_some_and(|active| {
                    active.playback() == crate::domain::state::PlaybackState::Live
                })
            });
            assert_eq!(app.model().active().unwrap().applied().settings(), &prior);
            assert_ne!(app.model().active().unwrap().attempt(), paused_owner);
            assert!(
                app.take_verified_applied().is_none(),
                "Resume has no user-save event"
            );
            let resumed = app.model().active().unwrap().attempt();
            app.restart(app.model().state_identity()).unwrap();
            native_app_wait(&mut app, "fresh complete LIVE restart proof", |app| {
                app.model()
                    .active()
                    .is_some_and(|active| active.attempt() != resumed)
            });
            assert_eq!(app.model().active().unwrap().applied().settings(), &prior);
            assert_eq!(app.model().draft().settings, newer);
            assert!(
                app.take_verified_applied().is_none(),
                "Restart has no user-save event"
            );
        }));
        app.quit();
        native_app_wait(
            &mut app,
            "complete lifecycle actual owner retirement",
            |app| app.model().shutdown_ready(),
        );
        let lifecycle_evidence = recorder.native_snapshot().unwrap();
        eprintln!(
            "native complete lifecycle evidence: {}",
            serde_json::to_string(&lifecycle_evidence).unwrap()
        );
        if let Err(payload) = result {
            resume_unwind(payload)
        }
        assert_native_confirmations(&lifecycle_evidence);
        let commands: Vec<_> = lifecycle_evidence
            .iter()
            .filter(|event| event["event"] == "command")
            .collect();
        assert_eq!(
            commands.len(),
            3,
            "startup/resume/restart each independently confirm, paused owner preparation is not proof"
        );
        for command in commands {
            assert!(command["argv"][2].as_str().unwrap().contains("@furami_0:"));
            assert!(command["argv"][2].as_str().unwrap().contains("@furami_2:"));
            assert!(!command["argv"][2].as_str().unwrap().contains("@furami_1:"));
        }
        let milestones = recorder.snapshot().unwrap();
        assert_eq!(
            milestones
                .iter()
                .filter(|event| matches!(event, FixtureMilestone::Created { .. }))
                .count(),
            4
        );
        assert_eq!(
            milestones
                .iter()
                .filter(|event| matches!(event, FixtureMilestone::Destroyed { .. }))
                .count(),
            4
        );

        for fault in [
            FixtureFault::CommandSubmission(-12),
            FixtureFault::CreationRejected,
        ] {
            let recorder = FixtureRecorder::default();
            let mut gate = native_filter_runner(
                prefix.clone(),
                normal.clone(),
                recorder.clone(),
                Some((fault, FilterPass::LiveCandidate)),
                false,
            );
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut requested = settings();
                requested.video.mode.size = FrameSize::new(320, 240).unwrap();
                requested.video.mode.rate = FrameRate::new(30, 1).unwrap();
                let compiled = gate.prepare_filters(&requested).unwrap();
                gate.begin_open(
                    key(1),
                    fixture_prepared(requested).unwrap(),
                    compiled,
                    PlaybackGain::default(),
                    InitialPlayback::Live,
                    output_fixture_plan(),
                )
                .unwrap();
                gate.take_native_update();
                gate.surface_ready(token(1));
                native_gate_event(&mut gate, "healthy fault incumbent", |event| match event {
                    SessionEvent::OpenVerified { .. } => Some(()),
                    SessionEvent::OpenFailed { failure, .. } => {
                        panic!("incumbent must open: {failure:?}")
                    }
                    _ => None,
                });
                let failure = native_submit_chain(&mut gate, 2, native_chain()).unwrap_err();
                assert!(matches!(
                    (&fault, &failure.kind),
                    (
                        FixtureFault::CommandSubmission(-12),
                        FilterErrorKind::CommandSubmission { mpv_error: -12 }
                    ) | (
                        FixtureFault::CreationRejected,
                        FilterErrorKind::CommandRejected { .. }
                    )
                ));
                assert!(
                    !failure.requires_fresh_owner,
                    "clean native rejection must preserve incumbent: {failure:?}"
                );
                assert_eq!(gate.phase(), GatePhase::Ready);
                assert!(gate.endpoint.is_some());
                native_submit_chain(&mut gate, 3, FilterChain::default())
                    .expect("same healthy owner must independently confirm after rejection");
                assert_eq!(
                    recorder
                        .snapshot()
                        .unwrap()
                        .iter()
                        .filter(|event| matches!(event, FixtureMilestone::Created { .. }))
                        .count(),
                    1
                );
            }));
            native_gate_retire(&mut gate);
            let evidence = recorder.native_snapshot().unwrap();
            eprintln!(
                "native synchronous fault evidence: {}",
                serde_json::to_string(&evidence).unwrap()
            );
            if let Err(payload) = result {
                resume_unwind(payload)
            }
            assert_native_confirmations(&evidence);
        }

        let recorder = FixtureRecorder::default();
        let mut gate =
            native_filter_runner(prefix.clone(), tiny.clone(), recorder.clone(), None, false);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut requested = settings();
            requested.video.mode.size = FrameSize::new(4, 4).unwrap();
            requested.video.mode.rate = FrameRate::new(30, 1).unwrap();
            let compiled = gate.prepare_filters(&requested).unwrap();
            gate.begin_open(
                key(1),
                fixture_prepared(requested).unwrap(),
                compiled,
                PlaybackGain::default(),
                InitialPlayback::Live,
                output_fixture_plan(),
            )
            .unwrap();
            gate.take_native_update();
            gate.surface_ready(token(1));
            native_gate_event(
                &mut gate,
                "tiny healthy owner before poisoning",
                |event| match event {
                    SessionEvent::OpenVerified { .. } => Some(()),
                    SessionEvent::OpenFailed { failure, .. } => {
                        panic!("tiny empty owner must open: {failure:?}")
                    }
                    _ => None,
                },
            );
            let failed = FilterChain::new(vec![FilterEntry::new(
                "retained failed bwdif".into(),
                Filter::Bwdif(BwdifParams::new(
                    BwdifMode::SendFrame,
                    FieldParity::Auto,
                    DeinterlaceSelection::All,
                )),
                true,
            )])
            .unwrap();
            let failure = native_submit_chain(&mut gate, 2, failed.clone()).unwrap_err();
            assert_eq!(failure.kind, FilterErrorKind::RuntimeGraph);
            assert!(failure.requires_fresh_owner);
            let installed_commands = recorder
                .native_snapshot()
                .unwrap()
                .iter()
                .filter(|event| event["event"] == "command")
                .count();
            let mut retained_entries = failed.entries().to_vec();
            retained_entries.push(FilterEntry::new(
                "new format retaining failed entry".into(),
                Filter::Format(FormatParams::new(
                    SdrMatrix::Auto,
                    ColorLevels::Auto,
                    SdrGamma::Auto,
                )),
                true,
            ));
            for (apply, chain) in [
                (3, failed),
                (4, FilterChain::new(retained_entries).unwrap()),
                (5, FilterChain::default()),
            ] {
                let denied = native_submit_chain(&mut gate, apply, chain).unwrap_err();
                assert_eq!(
                    denied.kind,
                    FilterErrorKind::Unconfirmed {
                        reason:
                            crate::domain::failure::FilterConfirmationFailure::BackendUnavailable,
                    },
                    "identical, retained-entry and empty replay must never confirm on poisoned owner"
                );
                assert!(denied.requires_fresh_owner);
                assert_eq!(
                    denied.diagnostics.key.unwrap().attempt,
                    gate.attempt().unwrap()
                );
            }
            assert_eq!(
                recorder
                    .native_snapshot()
                    .unwrap()
                    .iter()
                    .filter(|event| event["event"] == "command")
                    .count(),
                installed_commands,
                "poison denies all later native vf writes rather than attempting a reset"
            );
            assert_eq!(
                recorder
                    .snapshot()
                    .unwrap()
                    .iter()
                    .filter(|event| matches!(event, FixtureMilestone::Created { .. }))
                    .count(),
                1
            );
        }));
        native_gate_retire(&mut gate);
        let evidence = recorder.native_snapshot().unwrap();
        eprintln!(
            "native poisoned-owner replay evidence: {}",
            serde_json::to_string(&evidence).unwrap()
        );
        if let Err(payload) = result {
            resume_unwind(payload)
        }
        assert_native_confirmations(&evidence);

        for restore_fault in [false, true] {
            let recorder = FixtureRecorder::default();
            let gate = native_filter_runner(
                prefix.clone(),
                tiny.clone(),
                recorder.clone(),
                None,
                restore_fault,
            );
            let mut prior = settings();
            prior.video.mode.size = FrameSize::new(4, 4).unwrap();
            prior.video.mode.rate = FrameRate::new(30, 1).unwrap();
            let mut app = FixtureApp::new(
                prior.clone(),
                PlaybackGain::default(),
                FixtureValidator::default(),
                gate,
            );
            let result = catch_unwind(AssertUnwindSafe(|| {
                app.apply(app.model().state_identity(), app.model().draft().revision)
                    .unwrap();
                native_app_wait(&mut app, "tiny empty initial verified owner", |app| {
                    app.model().active().is_some()
                });
                let first = app.model().active().unwrap().attempt();
                app.take_verified_applied()
                    .expect("full initial LIVE receipt");
                let mut candidate = prior.clone();
                candidate.filters = FilterChain::new(vec![FilterEntry::new(
                    "tiny bwdif\ninert user label".into(),
                    Filter::Bwdif(BwdifParams::new(
                        BwdifMode::SendFrame,
                        FieldParity::Auto,
                        DeinterlaceSelection::All,
                    )),
                    true,
                )])
                .unwrap();
                let revision = app
                    .edit_draft(app.model().draft().revision, candidate.clone())
                    .unwrap();
                let crate::domain::state::ApplyAdmission::Filters {
                    key: admitted,
                    revision: admitted_revision,
                } = app.apply(app.model().state_identity(), revision).unwrap()
                else {
                    panic!("same-source bwdif must use the actual live coordinator path")
                };
                assert_eq!(admitted_revision, revision);
                assert_eq!(admitted.attempt, first);
                assert_eq!(admitted.pass, FilterPass::LiveCandidate);
                assert_eq!(app.model().phase(), ProductPhase::ApplyingFilters);
                assert_eq!(
                    app.validator_mut().requests.len(),
                    1,
                    "live candidate performs no capture validation/open"
                );
                let mut newer = candidate.clone();
                newer.filters.set_enabled(0, false).unwrap();
                app.edit_draft(revision, newer.clone()).unwrap();
                native_app_wait(
                    &mut app,
                    "delayed tiny graph failure and one fresh restoration",
                    |app| {
                        matches!(
                            app.model().phase(),
                            ProductPhase::ErrorWithActiveRestored
                                | ProductPhase::ErrorWithoutActive
                        ) && matches!(
                            app.model().cleanup(),
                            crate::domain::state::CleanupStatus::Complete
                        )
                    },
                );
                let failures = app
                    .model()
                    .failures()
                    .expect("candidate failure retained")
                    .clone();
                let filter = failures
                    .candidate
                    .as_ref()
                    .and_then(|failure| failure.filter.as_ref())
                    .expect("tiny candidate must retain typed filter cause");
                assert_eq!(
                    filter.kind,
                    FilterErrorKind::RuntimeGraph,
                    "successful command followed by asynchronous graph failure is required"
                );
                assert!(filter.requires_fresh_owner);
                assert_eq!(filter.diagnostics.key, Some(admitted));
                let evidence = recorder.native_snapshot().unwrap();
                let key = serde_json::to_value(filter.diagnostics.key.unwrap()).unwrap();
                let reply = evidence
                    .iter()
                    .position(|event| {
                        event["event"] == "reply"
                            && event["key"] == key
                            && event["error"].as_i64().is_some_and(|error| error >= 0)
                    })
                    .expect("tiny domain-valid vf set must genuinely reply success");
                assert!(
                    evidence[reply + 1..].iter().any(|event| {
                        event["event"] == "log"
                            && event["key"] == key
                            && event["text"].as_str().is_some_and(|text| {
                                text.contains("Disabling filter")
                                    || text.contains("failed to configure the filter graph")
                                    || text.contains("could not initialize filter pads")
                            })
                    }),
                    "measured delayed graph/disable evidence must follow successful reply"
                );
                assert!(
                    filter.attributed_ordinal.is_none_or(|ordinal| ordinal == 0),
                    "only proven enabled internal label may attribute"
                );
                assert_eq!(
                    app.model().draft().settings,
                    newer,
                    "newer edited draft must survive failed frozen candidate"
                );
                assert_eq!(
                    failures.candidate.as_ref().unwrap().requested.as_ref(),
                    &candidate
                );
                assert_eq!(app.model().last_valid().unwrap().settings(), &prior);
                assert!(
                    app.take_verified_applied().is_none(),
                    "a restoration cannot mint a user applied receipt"
                );
                assert!(!app.has_user_apply_result_or_pending(admitted.apply));
                if restore_fault {
                    assert_eq!(app.model().phase(), ProductPhase::ErrorWithoutActive);
                    assert!(app.model().active().is_none());
                    assert!(
                        failures
                            .restore
                            .as_ref()
                            .and_then(|failure| failure.filter.as_ref())
                            .is_some()
                    );
                } else {
                    assert_eq!(app.model().phase(), ProductPhase::ErrorWithActiveRestored);
                    let restored = app.model().active().unwrap();
                    assert_ne!(
                        restored.attempt(),
                        first,
                        "empty restoration must use fresh actual owner"
                    );
                    assert_eq!(restored.applied().settings(), &prior);
                }
                assert_eq!(
                    app.validator_mut().requests.len(),
                    2,
                    "initial and exactly one fresh Restore"
                );
                assert_eq!(
                    recorder
                        .snapshot()
                        .unwrap()
                        .iter()
                        .filter(|event| matches!(event, FixtureMilestone::Created { .. }))
                        .count(),
                    2
                );
                eprintln!(
                    "native tiny application outcome: state={:?}, failures={:?}",
                    app.model().state_identity(),
                    app.model().failures()
                );
            }));
            app.quit();
            native_app_wait(&mut app, "native tiny Quit retirement", |app| {
                app.model().shutdown_ready()
            });
            let milestones = recorder.snapshot().unwrap();
            assert_eq!(
                milestones
                    .iter()
                    .filter(|event| matches!(event, FixtureMilestone::Created { .. }))
                    .count(),
                milestones
                    .iter()
                    .filter(|event| matches!(event, FixtureMilestone::Destroyed { .. }))
                    .count()
            );
            let evidence = recorder.native_snapshot().unwrap();
            eprintln!(
                "native tiny raw evidence restore_fault={restore_fault}: {}",
                serde_json::to_string(&evidence).unwrap()
            );
            if let Err(payload) = result {
                resume_unwind(payload)
            }
            assert_native_confirmations(&evidence);
        }
        eprintln!(
            "native contract qualifies finite null-output owner/gate/app evidence only, not physical display, arbitrary future frames, audio or Qt surface lifecycle"
        );
    }
}
