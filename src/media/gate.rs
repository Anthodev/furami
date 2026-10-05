//! Concrete one-owner SessionRunner. Native gate never chooses replacement policy.

use super::{
    controller::{
        Generation, MediaError, OwnerEndpoint, PlaybackIntent, SessionConfig, Snapshot,
        SurfaceToken,
    },
    session::{SessionFacts, VerificationStatus},
};
use crate::{
    app::{
        gate::{GatePhase, GateState, GateUpdate},
        ports::{
            AudioOutcome, FactStatus, ImmediateIntent, OpenReceipt, SessionEvent, SessionRunner,
            StartFailure, StopReason, StopSubmission, SubmitStatus, VerificationSummary,
        },
    },
    capture::PreparedCapture,
    domain::{
        capture::{
            AudioAvailability, AudioEpoch, AudioSelection, AudioSilence, PlaybackGain, WatchStamp,
        },
        failure::{ApplyFailure, Cause, FailureCategory, LifecycleFailure, Stage},
        state::{AttemptId, AttemptKey, DraftSettings, InitialPlayback, PauseRequestId},
    },
};

type Spawner = dyn FnMut(Generation, SessionConfig) -> Result<OwnerEndpoint, MediaError>;

pub(crate) struct GateRunner {
    state: GateState,
    endpoint: Option<OwnerEndpoint>,
    spawn: Box<Spawner>,
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
    pub(crate) fn new(prefix: String) -> Self {
        Self::with_spawner(move |generation, config| {
            OwnerEndpoint::spawn(generation, prefix.clone(), config)
        })
    }
    pub(crate) fn with_spawner(
        spawn: impl FnMut(Generation, SessionConfig) -> Result<OwnerEndpoint, MediaError> + 'static,
    ) -> Self {
        Self {
            state: GateState::new(),
            endpoint: None,
            spawn: Box::new(spawn),
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
    fn snapshot(&mut self, snapshot: Snapshot) -> Option<SessionEvent> {
        let key = self.key?;
        if snapshot.generation.get() != key.attempt.get()
            || matches!(
                self.state.cleanup(),
                GatePhase::Releasing | GatePhase::Stopping
            )
        {
            return None;
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
            if let Some(failure) = self.map_failure(error.clone()) {
                self.emit_failure(failure);
            }
            return self
                .stream_event
                .take()
                .or_else(|| self.failure_event.take());
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
            return self.stream_event.take();
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
            snapshot.readiness.is_some(),
        );
        self.apply_native(update);
        if !self.opening_result_sent && self.state.phase() == GatePhase::Ready {
            match verified_receipt(self.requested.as_ref()?, &snapshot) {
                Ok(Some(receipt)) => {
                    self.opening_result_sent = true;
                    self.verified = true;
                    return Some(SessionEvent::OpenVerified { key, receipt });
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
    fn begin_open(
        &mut self,
        key: AttemptKey,
        prepared: PreparedCapture,
        gain: PlaybackGain,
        playback: InitialPlayback,
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
            watch,
            selected_route,
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
            ImmediateIntent::SetGain(_)
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
}

fn fact_status(status: VerificationStatus) -> FactStatus {
    match status {
        VerificationStatus::Unverified => FactStatus::Unverified,
        VerificationStatus::ObservedCompatible => FactStatus::ObservedCompatible,
        VerificationStatus::Approximate => FactStatus::Approximate,
    }
}
/// Uses existing verifier, never weakens replay predicate or invents missing facts.
fn verified_receipt(
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
        (
            AudioSelection::Enabled { source },
            AudioAvailability::Active {
                source: actual,
                route,
            },
        ) if source == actual
            && snapshot.audio_epoch == Some(route.epoch)
            && route.source_index != u32::MAX
            && route.source_output_index != u32::MAX
            && route.client_index != u32::MAX =>
        {
            AudioOutcome::Active {
                source: source.clone(),
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
    Ok(Some(OpenReceipt {
        settings: settings.clone(),
        verification: VerificationSummary {
            captured_fourcc: fact_status(verified.verification.captured_fourcc),
            decoded_size: fact_status(verified.verification.decoded_size),
            nominal_rate: fact_status(verified.verification.nominal_rate),
        },
        audio,
        readiness,
    }))
}

fn log_session_event(
    key: Option<AttemptKey>,
    requested: Option<&DraftSettings>,
    event: &SessionEvent,
) {
    let (name, attempt) = match event {
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
                AudioOutcome::Active { source, route } => serde_json::json!({"Active": {"source": source, "route": route}}),
                AudioOutcome::Silent { source, reason } => serde_json::json!({"Silent": {"source": source, "reason": reason}}),
            },
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
        let runner = GateRunner::with_spawner(move |generation, config| {
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
                FixtureBackend::new(
                    prefix,
                    fixture,
                    input,
                    config.gain,
                    config.playback,
                    config.watch,
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
        let runner = GateRunner::with_spawner(move |generation, session| {
            let mode = session.video.requested().mode;
            let input = session
                .video
                .validate_snapshot(&session_fixture(&["/dev/video0"], mode))
                .unwrap();
            let requested = input.requested().clone();
            let (driver, backend) = Driver::pair(config.take().unwrap_or_default());
            tx.send(driver).unwrap();
            OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || backend)
        });
        (runner, rx)
    }
    fn open(runner: &mut GateRunner, value: u64) {
        runner
            .begin_open(
                key(value),
                fixture_prepared(settings()).unwrap(),
                PlaybackGain::default(),
                InitialPlayback::Live,
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
                    PlaybackGain::default(),
                    InitialPlayback::Live,
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
        let mut runner = GateRunner::with_spawner(|_, _| {
            Err(MediaError::new("owner_spawn", "injected spawn failure"))
        });
        assert!(matches!(
            runner.begin_open(
                key(1),
                fixture_prepared(settings()).unwrap(),
                PlaybackGain::default(),
                InitialPlayback::Live,
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
    fn actual_quiescence_completion_required_after_native_destroy() {
        let (mut runner, drivers) = runner(Config {
            hold_quiesce: true,
            creates_handle: true,
            ..Config::default()
        });
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        ready(&mut runner, &driver, 1);
        runner.stop(key(1).attempt, StopReason::Replace);
        driver.destroyed.recv().unwrap();
        driver.quiesce_started.recv().unwrap();
        assert!(!runner.take_native_update().release_native);
        assert!(runner.poll().is_none());
        assert!(
            runner
                .begin_open(
                    key(2),
                    fixture_prepared(settings()).unwrap(),
                    PlaybackGain::default(),
                    InitialPlayback::Live,
                )
                .is_err()
        );
        driver.quiesce_release.send(()).unwrap();
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
                    PlaybackGain::default(),
                    InitialPlayback::Live,
                )
                .is_err()
        );
    }
    #[test]
    fn missing_owner_ack_never_releases_native_or_allows_next_owner() {
        let mut runner = GateRunner::with_spawner(|generation, _| {
            OwnerEndpoint::spawn_with_backend(
                generation,
                || -> crate::media::controller::test_support::FakeBackend {
                    panic!("injected factory panic")
                },
            )
        });
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
        let receipt = verified_receipt(&settings, &snapshot).unwrap().unwrap();
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
        assert!(verified_receipt(&enabled, &snapshot).is_err());
        snapshot.audio = AudioAvailability::Opening {
            epoch: AudioEpoch::new(1).unwrap(),
        };
        assert!(verified_receipt(&enabled, &snapshot).unwrap().is_none());
        let route = crate::domain::capture::AudioRouteReceipt {
            epoch: AudioEpoch::new(1).unwrap(),
            stamp: crate::domain::capture::WatchStamp {
                watch: crate::domain::capture::WatchId::new(1).unwrap(),
                epoch: crate::domain::capture::ObservationEpoch::new(1).unwrap(),
            },
            source_index: 3,
            source_output_index: 4,
            client_index: 5,
        };
        snapshot.audio_epoch = Some(route.epoch);
        snapshot.audio = AudioAvailability::Active {
            source: source.clone(),
            route: route.clone(),
        };
        assert_eq!(
            verified_receipt(&enabled, &snapshot)
                .unwrap()
                .unwrap()
                .audio,
            AudioOutcome::Active { source, route }
        );
        snapshot.session = None;
        assert!(verified_receipt(&enabled, &snapshot).unwrap().is_none());
    }
    #[test]
    fn contradictions_and_wrong_prepared_settings_never_get_receipt() {
        let settings = settings();
        let mut snapshot = snapshot(&settings, AudioAvailability::Disabled);
        snapshot.session.as_mut().unwrap().observed.decoded_size =
            Some(super::super::session::Observation {
                value: FrameSize::new(1920, 1080).unwrap(),
                source: super::super::session::Source::MpvDecodedParams,
            });
        assert_eq!(
            verified_receipt(&settings, &snapshot).unwrap_err().stage,
            Stage::Verification
        );
        let mut different = settings.clone();
        different.video.mode.rate = FrameRate::new(30, 1).unwrap();
        assert!(verified_receipt(&different, &snapshot).is_err());
    }
    #[test]
    fn missing_facts_stay_unverified_and_nominal_report_approximate() {
        let settings = settings();
        let mut snapshot = snapshot(&settings, AudioAvailability::Disabled);
        let receipt = verified_receipt(&settings, &snapshot).unwrap().unwrap();
        assert_eq!(receipt.verification.decoded_size, FactStatus::Unverified);
        assert_eq!(receipt.verification.nominal_rate, FactStatus::Unverified);
        snapshot.session.as_mut().unwrap().observed.nominal_rate =
            Some(super::super::session::Observation {
                value: 60.0,
                source: super::super::session::Source::MpvContainerFps,
            });
        assert_eq!(
            verified_receipt(&settings, &snapshot)
                .unwrap()
                .unwrap()
                .verification
                .nominal_rate,
            FactStatus::Approximate
        );
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
        let receipt = verified_receipt(&enabled, &progress).unwrap().unwrap();
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
        assert!(verified_receipt(&enabled, &progress).unwrap().is_none());
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
}
