//! Concrete one-owner SessionRunner. Native gate never chooses replacement policy.

use super::{
    controller::{
        AudioStatus, Generation, MediaError, OwnerEndpoint, PlaybackIntent, SessionConfig,
        Snapshot, SurfaceToken,
    },
    session::{SessionFacts, VerificationStatus},
};
use crate::{
    app::{
        gate::{GatePhase, GateState, GateUpdate},
        ports::{
            AudioDiagnostic, AudioOutcome, FactStatus, ImmediateIntent, OpenReceipt, SessionEvent,
            SessionRunner, StartFailure, StopReason, StopSubmission, SubmitStatus,
            VerificationSummary,
        },
    },
    capture::PreparedCapture,
    domain::{
        capture::{AudioSelection, PlaybackGain},
        failure::{ApplyFailure, Cause, FailureCategory, LifecycleFailure, Stage},
        state::{AttemptId, AttemptKey, DraftSettings},
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
    create_native: bool,
    release_native: bool,
    dirty: bool,
    last_blocked: Option<ApplyFailure>,
    fatal_native: Option<ApplyFailure>,
    pub(crate) paused: bool,
    pub(crate) ended: bool,
    pub(crate) audio: AudioStatus,
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
            create_native: false,
            release_native: false,
            dirty: true,
            last_blocked: None,
            fatal_native: None,
            paused: false,
            ended: false,
            audio: AudioStatus::Disabled,
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
        if let Some(error) = snapshot.failure {
            if let Some(failure) = self.map_failure(error) {
                self.emit_failure(failure);
            }
            return self.failure_event.take();
        }
        self.dirty |= self.paused != snapshot.paused
            || self.ended != snapshot.ended
            || self.audio != snapshot.audio;
        self.paused = snapshot.paused;
        self.ended = snapshot.ended;
        if self.audio != snapshot.audio {
            self.audio_event = Some(SessionEvent::AudioDiagnostic {
                attempt: key.attempt,
                status: audio_diagnostic(&snapshot.audio),
            });
            self.audio = snapshot.audio.clone();
        }
        if let Some(session) = &snapshot.session {
            let report = session.summary();
            self.dirty |= self.report != report;
            self.report = report;
        }
        if snapshot.ended && !self.opening_result_sent {
            let failure = ApplyFailure::new(
                FailureCategory::Session,
                Stage::StreamStart,
                Cause::Generic,
                self.requested.clone()?,
                "session_ended",
                "capture ended before verified opening could commit",
            );
            self.emit_failure(failure);
            return self.failure_event.take();
        }
        let update =
            self.state
                .progress(key.attempt, snapshot.initialized, snapshot.playback_started);
        self.apply_native(update);
        if !self.opening_result_sent && self.state.phase() == GatePhase::Ready {
            if let AudioStatus::RestartRequired(error) = &snapshot.audio {
                let failure = ApplyFailure::new(
                    FailureCategory::Session,
                    Stage::Open,
                    Cause::Generic,
                    self.requested.clone()?,
                    "audio_open",
                    error.to_string(),
                );
                self.emit_failure(failure);
                return self.failure_event.take();
            }
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
        if snapshot.ended && self.verified && !self.ended_sent {
            self.ended_sent = true;
            return Some(SessionEvent::SessionEnded {
                attempt: key.attempt,
            });
        }
        self.audio_event.take()
    }
}

impl SessionRunner for GateRunner {
    type Prepared = PreparedCapture;
    fn begin_open(
        &mut self,
        key: AttemptKey,
        prepared: PreparedCapture,
        gain: PlaybackGain,
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
        let (_, video, _, _) = prepared.into_parts();
        let config = SessionConfig {
            video,
            audio: requested.audio.clone(),
            gain,
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
        self.last_blocked = None;
        self.paused = false;
        self.ended = false;
        self.audio = if self
            .requested
            .as_ref()
            .is_some_and(|settings| settings.audio.enabled())
        {
            AudioStatus::Opening
        } else {
            AudioStatus::Disabled
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
                    self.audio = AudioStatus::Disabled;
                    self.audio_event = None;
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
            if let Some(snapshot) = self
                .endpoint
                .as_ref()
                .and_then(OwnerEndpoint::take_snapshot)
            {
                return self.snapshot(snapshot);
            }
            self.audio_event.take()
        })();
        if let Some(event) = &event {
            log_session_event(self.key, self.requested.as_ref(), event);
        }
        if matches!(&event, Some(SessionEvent::NativeReleased { .. })) {
            self.key = None;
            self.requested = None;
            self.audio_event = None;
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
        if self.state.phase() != GatePhase::Ready || !self.state.has_surface() {
            return SubmitStatus::NotReady;
        }
        let Some(generation) = Generation::new(attempt.get()) else {
            return SubmitStatus::StaleGeneration;
        };
        let intent = match intent {
            ImmediateIntent::TogglePause => PlaybackIntent::TogglePause,
            ImmediateIntent::SetGain(gain) => PlaybackIntent::SetGain(gain),
        };
        let status = self
            .endpoint
            .as_ref()
            .map(|endpoint| endpoint.submit(generation, intent))
            .unwrap_or(super::controller::SubmitStatus::Closing);
        let mapped = match status {
            super::controller::SubmitStatus::Accepted => SubmitStatus::Accepted,
            super::controller::SubmitStatus::StaleGeneration => SubmitStatus::StaleGeneration,
            super::controller::SubmitStatus::NotReady => SubmitStatus::NotReady,
            super::controller::SubmitStatus::Closing => SubmitStatus::Closing,
            super::controller::SubmitStatus::CapacityExceeded => SubmitStatus::CapacityExceeded,
        };
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

fn audio_diagnostic(audio: &AudioStatus) -> AudioDiagnostic {
    match audio {
        AudioStatus::Disabled => AudioDiagnostic::Disabled,
        AudioStatus::Opening => AudioDiagnostic::Opening,
        AudioStatus::Active => AudioDiagnostic::Active,
        AudioStatus::RestartRequired(error) => AudioDiagnostic::RestartRequired(error.clone()),
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
        (AudioSelection::Disabled { .. }, AudioStatus::Disabled) => AudioOutcome::Disabled,
        (AudioSelection::Enabled { source }, AudioStatus::Active) => AudioOutcome::Active {
            source: source.clone(),
        },
        (_, AudioStatus::Opening) => return Ok(None),
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
        SessionEvent::SessionEnded { attempt } => ("SessionEnded", *attempt),
        SessionEvent::OwnerStopped { attempt, .. } => ("OwnerStopped", *attempt),
        SessionEvent::NativeReleased { attempt } => ("NativeReleased", *attempt),
        SessionEvent::CleanupBlocked { attempt, .. } => ("CleanupBlocked", *attempt),
        SessionEvent::AudioDiagnostic { attempt, .. } => ("AudioDiagnostic", *attempt),
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
                AudioOutcome::Active { source } => serde_json::json!({"Active": {"source": source}}),
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
            )
            .unwrap();
        assert!(runner.take_native_update().create_native);
    }
    fn ready(runner: &mut GateRunner, driver: &Driver, value: u64) {
        runner.surface_ready(token(value));
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        assert!(
            matches!(runner.poll(), Some(SessionEvent::OpenVerified { key: received, .. }) if received == key(value))
        );
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
            runner.submit_immediate(key(1).attempt, ImmediateIntent::TogglePause),
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
                    PlaybackGain::default()
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
            runner.submit_immediate(key(1).attempt, ImmediateIntent::TogglePause),
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
                PlaybackGain::default()
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
                    PlaybackGain::default()
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
            ..Config::default()
        });
        open(&mut runner, 1);
        let driver = drivers.recv().unwrap();
        ready(&mut runner, &driver, 1);
        for _ in 0..64 {
            assert_eq!(
                runner.submit_immediate(key(1).attempt, ImmediateIntent::TogglePause),
                SubmitStatus::Accepted
            );
        }
        assert_eq!(
            runner.submit_immediate(key(1).attempt, ImmediateIntent::TogglePause),
            SubmitStatus::CapacityExceeded
        );
        assert!(
            matches!(runner.poll(), Some(SessionEvent::SessionFailed { failure, .. }) if failure.operation == "command_overflow")
        );
        assert!(!runner.take_native_update().release_native);
        assert_eq!(
            runner.submit_immediate(key(1).attempt, ImmediateIntent::TogglePause),
            SubmitStatus::Closing
        );
        driver.shutdown_release.send(()).unwrap();
        stopped(&mut runner);
        driver.destroyed.recv().unwrap();
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
                    PlaybackGain::default()
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
    fn snapshot(settings: &DraftSettings, audio: AudioStatus) -> Snapshot {
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
            paused: false,
            ended: false,
            failure: None,
            audio,
        }
    }
    #[test]
    fn receipt_requires_session_facts_and_exact_enabled_audio_outcome() {
        let settings = settings();
        let mut snapshot = snapshot(&settings, AudioStatus::Disabled);
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
        snapshot.audio = AudioStatus::Opening;
        assert!(verified_receipt(&enabled, &snapshot).unwrap().is_none());
        snapshot.audio = AudioStatus::Active;
        assert_eq!(
            verified_receipt(&enabled, &snapshot)
                .unwrap()
                .unwrap()
                .audio,
            AudioOutcome::Active { source }
        );
        snapshot.session = None;
        assert!(verified_receipt(&enabled, &snapshot).unwrap().is_none());
    }
    #[test]
    fn contradictions_and_wrong_prepared_settings_never_get_receipt() {
        let settings = settings();
        let mut snapshot = snapshot(&settings, AudioStatus::Disabled);
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
        let mut snapshot = snapshot(&settings, AudioStatus::Disabled);
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
}

#[cfg(test)]
mod opening_end_regression {
    use super::*;
    use crate::{
        capture::{apply::fixture_prepared, linux::session_fixture},
        domain::{
            capture::{CaptureMode, CapturedFourCc, FrameRate, FrameSize, ModeRequest},
            state::{ApplyId, AttemptPurpose},
        },
        media::controller::{
            BackendEvent, X11WindowId,
            test_support::{Config, Driver},
        },
    };
    use std::sync::mpsc;

    #[test]
    fn ended_coalesced_with_ready_fails_open_instead_of_committing() {
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(1280, 720).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        let fixture = session_fixture(&["/dev/video0"], mode);
        let settings = DraftSettings {
            video: ModeRequest {
                identity: fixture.devices()[0].identity().clone(),
                mode,
            },
            audio: AudioSelection::default(),
        };
        let (tx, rx) = mpsc::channel();
        let mut runner = GateRunner::with_spawner(move |generation, config| {
            let requested = config.video.requested();
            let input = config
                .video
                .validate_snapshot(&session_fixture(&["/dev/video0"], requested.mode))
                .unwrap();
            let requested = input.requested().clone();
            let (driver, backend) = Driver::pair(Config::default());
            tx.send(driver).unwrap();
            OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || backend)
        });
        let key = AttemptKey {
            apply: ApplyId::new(1).unwrap(),
            attempt: AttemptId::new(1).unwrap(),
            purpose: AttemptPurpose::Candidate,
        };
        runner
            .begin_open(
                key,
                fixture_prepared(settings).unwrap(),
                PlaybackGain::default(),
            )
            .unwrap();
        runner.take_native_update();
        let driver = rx.recv().unwrap();
        runner.surface_ready(SurfaceToken {
            generation: Generation::new(1).unwrap(),
            xid: X11WindowId::new(71).unwrap(),
        });
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        driver.send(BackendEvent::PlaybackRestart);
        driver.send(BackendEvent::EndFile {
            reason: 0,
            error: 0,
        });
        driver.fence();
        assert!(
            matches!(runner.poll(), Some(SessionEvent::OpenFailed { failure, .. }) if failure.operation == "session_ended")
        );
        runner.endpoint.as_mut().unwrap().wait_for_ack().unwrap();
        assert!(matches!(
            runner.poll(),
            Some(SessionEvent::OwnerStopped { .. })
        ));
        assert!(runner.take_native_update().release_native);
        driver.destroyed.recv().unwrap();
    }
}
