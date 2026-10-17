//! Dedicated file qualification lifetime, without a capture identity or recovery policy.
use crate::media::{
    controller::{
        Generation, MediaError, OwnerEndpoint, PlaybackIntent, ReadinessFacts, Snapshot,
        SubmitStatus, SurfaceToken,
    },
    qualification::QualificationVideo,
    render_catalog::CompiledRendererSettings,
    session::ObservedFacts,
};
use crate::{
    app::gate::GatePhase,
    domain::state::{AttemptId, PauseRequestId},
};

pub(crate) struct QualificationUpdate {
    pub phase: GatePhase,
    pub generation: u64,
    pub create_native: bool,
    pub release_native: bool,
    pub can_toggle_pause: bool,
    pub paused: bool,
    pub observations: Option<ObservedFacts>,
    pub diagnostics: Option<crate::media::controller::RuntimeMediaObservation>,
    pub terminal_failure: Option<MediaError>,
    pub quit: bool,
}
pub(crate) struct QualificationGate {
    phase: GatePhase,
    endpoint: Option<OwnerEndpoint>,
    opened: bool,
    closing: bool,
    create_native: bool,
    release_native: bool,
    acknowledged: bool,
    pause_pending: Option<(PauseRequestId, bool)>,
    pause_sequence: u64,
    paused: bool,
    observations: Option<ObservedFacts>,
    diagnostics: Option<crate::media::controller::RuntimeMediaObservation>,
    failure: Option<MediaError>,
}
impl QualificationGate {
    pub(crate) fn new() -> Self {
        Self {
            phase: GatePhase::Idle,
            endpoint: None,
            opened: false,
            closing: false,
            create_native: false,
            release_native: false,
            acknowledged: false,
            pause_pending: None,
            pause_sequence: 0,
            paused: false,
            observations: None,
            diagnostics: None,
            failure: None,
        }
    }
    pub(crate) fn begin(
        &mut self,
        prefix: String,
        video: QualificationVideo,
        renderer: CompiledRendererSettings,
    ) -> Result<(), MediaError> {
        if self.phase != GatePhase::Idle {
            return Err(MediaError::new(
                "qualification_state",
                "qualification already started",
            ));
        }
        self.endpoint = Some(OwnerEndpoint::spawn_qualification(
            Generation::new(1).expect("fixed generation"),
            prefix,
            video,
            renderer,
        )?);
        self.phase = GatePhase::WaitingSurface;
        self.create_native = true;
        Ok(())
    }
    fn matches(&self, attempt: AttemptId) -> bool {
        attempt.get() == 1
    }
    pub(crate) fn surface_ready(&mut self, token: SurfaceToken) {
        if token.generation.get() != 1 || self.phase != GatePhase::WaitingSurface || self.closing {
            return;
        }
        if self
            .endpoint
            .as_ref()
            .is_some_and(|owner| owner.attach(token) == SubmitStatus::Accepted)
        {
            self.phase = GatePhase::Opening;
        } else {
            self.fail(MediaError::new(
                "surface_handoff",
                "qualification surface handoff refused",
            ));
        }
    }
    pub(crate) fn surface_lost(&mut self, attempt: AttemptId) {
        if self.matches(attempt)
            && !matches!(self.phase, GatePhase::Releasing | GatePhase::QuitReady)
        {
            self.fail(MediaError::new(
                "surface_lost",
                "qualification surface lost before owner acknowledgement",
            ));
        }
    }
    pub(crate) fn wait_for_owner_ack(&mut self, attempt: AttemptId) -> Result<(), MediaError> {
        if !self.matches(attempt) {
            return Err(MediaError::new(
                "stale_generation",
                "stale qualification surface",
            ));
        }
        let result = self
            .endpoint
            .as_mut()
            .ok_or_else(|| MediaError::new("owner_disconnect", "qualification owner absent"))?
            .wait_for_ack();
        if let Err(error) = &result {
            self.failure.get_or_insert_with(|| error.clone());
        }
        result
    }
    pub(crate) fn native_released(&mut self, attempt: AttemptId) {
        if self.matches(attempt) && self.phase == GatePhase::Releasing && self.acknowledged {
            self.endpoint = None;
            self.phase = GatePhase::QuitReady;
        }
    }
    fn fail(&mut self, error: MediaError) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        self.request_close();
    }
    pub(crate) fn request_close(&mut self) {
        self.closing = true;
        self.pause_pending = None;
        if let Some(owner) = &self.endpoint {
            if !self.acknowledged {
                owner.stop(
                    Generation::new(1).expect("fixed generation"),
                    self.failure.clone(),
                );
                self.phase = GatePhase::Stopping;
            }
        } else {
            self.phase = GatePhase::QuitReady;
        }
    }
    fn observe(&mut self, snapshot: Snapshot) {
        if let Some(error) = snapshot.failure {
            self.fail(error);
            return;
        }
        if snapshot.stream_ended.is_some() {
            self.fail(MediaError::new(
                "qualification_eof",
                "unexpected qualification EOF",
            ));
            return;
        }
        self.diagnostics = snapshot.diagnostics;
        if let Some(ReadinessFacts::QualificationVideo(facts)) = snapshot.session {
            self.observations = Some(facts);
        }
        if self.closing {
            return;
        }
        if snapshot.readiness == Some(crate::app::ports::OpenReadiness::Live) {
            self.opened = true;
            self.phase = GatePhase::Ready;
        }
        if let Some(pause) = snapshot.pause {
            if let Some((request, expected)) = self.pause_pending {
                if !snapshot.pause_pending
                    && pause.request == Some(request)
                    && pause.paused == expected
                {
                    self.paused = expected;
                    self.pause_pending = None;
                }
            } else {
                self.paused = pause.paused;
            }
        }
    }
    pub(crate) fn toggle_pause(&mut self, attempt: AttemptId) -> SubmitStatus {
        // Drain terminal observations before considering the sticky opening latch.
        self.collect();
        if !self.matches(attempt) {
            return SubmitStatus::StaleGeneration;
        }
        if self.closing || self.failure.is_some() {
            return SubmitStatus::Closing;
        }
        if !self.opened || self.pause_pending.is_some() {
            return SubmitStatus::NotReady;
        }
        let Some(next) = self
            .pause_sequence
            .checked_add(1)
            .and_then(PauseRequestId::new)
        else {
            self.fail(MediaError::new(
                "request_exhausted",
                "qualification pause sequence exhausted",
            ));
            return SubmitStatus::Closing;
        };
        let paused = !self.paused;
        let status = self
            .endpoint
            .as_ref()
            .map_or(SubmitStatus::Closing, |owner| {
                owner.submit(
                    Generation::new(1).expect("fixed generation"),
                    PlaybackIntent::SetPaused {
                        request: next,
                        paused,
                    },
                )
            });
        if status == SubmitStatus::Accepted {
            self.pause_sequence = next.get();
            self.pause_pending = Some((next, paused));
        }
        status
    }
    fn collect(&mut self) {
        if let Some(snapshot) = self
            .endpoint
            .as_ref()
            .and_then(OwnerEndpoint::take_snapshot)
        {
            self.observe(snapshot);
        }
        let stopped = self.endpoint.as_mut().map(OwnerEndpoint::take_stopped);
        match stopped {
            Some(Ok(Some(ack))) if ack.generation.get() == 1 => {
                if let Err(error) = ack.outcome {
                    self.failure.get_or_insert(error);
                }
                if !self.closing {
                    self.failure.get_or_insert_with(|| {
                        MediaError::new("owner_stopped", "qualification owner stopped unexpectedly")
                    });
                }
                self.closing = true;
                self.acknowledged = true;
                self.phase = GatePhase::Releasing;
                self.release_native = true;
            }
            Some(Err(error)) => self.fail(error),
            _ => {}
        }
    }
    pub(crate) fn poll(&mut self) -> QualificationUpdate {
        self.collect();
        QualificationUpdate {
            phase: self.phase,
            generation: 1,
            create_native: std::mem::take(&mut self.create_native),
            release_native: std::mem::take(&mut self.release_native),
            can_toggle_pause: self.opened
                && !self.closing
                && self.failure.is_none()
                && self.pause_pending.is_none(),
            paused: self.paused,
            observations: self.observations.clone(),
            diagnostics: self.diagnostics.clone(),
            terminal_failure: self.failure.clone(),
            quit: self.phase == GatePhase::QuitReady,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capture::FrameSize;
    use crate::media::{
        controller::{
            BackendCommand, BackendEvent, PauseObservation, X11WindowId,
            test_support::{Config, Driver},
        },
        session::{Observation, Source},
    };
    use std::time::{Duration, Instant};

    fn token(value: u64) -> SurfaceToken {
        SurfaceToken {
            generation: Generation::new(value).unwrap(),
            xid: X11WindowId::new(47).unwrap(),
        }
    }
    fn attempt(value: u64) -> AttemptId {
        AttemptId::new(value).unwrap()
    }
    fn fixture(config: Config) -> (QualificationGate, Driver) {
        let (driver, backend) = Driver::pair(config);
        let mut gate = QualificationGate::new();
        gate.endpoint = Some(
            OwnerEndpoint::spawn_qualification_with_backend(
                Generation::new(1).unwrap(),
                FrameSize::new(320, 180).unwrap(),
                backend,
            )
            .unwrap(),
        );
        gate.phase = GatePhase::WaitingSurface;
        gate.create_native = true;
        (gate, driver)
    }
    fn until(
        gate: &mut QualificationGate,
        predicate: impl Fn(&QualificationUpdate) -> bool,
    ) -> QualificationUpdate {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let update = gate.poll();
            if predicate(&update) {
                return update;
            }
            assert!(
                Instant::now() < deadline,
                "qualification gate did not reach expected state"
            );
            std::thread::yield_now();
        }
    }
    fn live(gate: &mut QualificationGate, driver: &Driver) {
        gate.surface_ready(token(1));
        driver
            .initialized
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let (load, command) = driver
            .submitted
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(command, BackendCommand::LoadInput);
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        driver.send(BackendEvent::Observed {
            started: true,
            facts: ObservedFacts {
                decoded_size: Some(Observation {
                    value: FrameSize::new(320, 180).unwrap(),
                    source: Source::MpvDecodedParams,
                }),
                ..Default::default()
            },
        });
        until(gate, |update| update.can_toggle_pause);
    }
    fn finish(gate: &mut QualificationGate, driver: &Driver) {
        gate.request_close();
        let release = until(gate, |update| update.release_native);
        assert!(!release.quit);
        driver
            .destroyed
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        gate.native_released(attempt(2));
        assert!(!gate.poll().quit);
        gate.native_released(attempt(1));
        assert!(gate.poll().quit);
    }
    #[test]
    fn pause_confirmation_clears_live_readiness_but_resume_uses_same_owner() {
        let (mut gate, driver) = fixture(Config::default());
        live(&mut gate, &driver);
        assert_eq!(gate.toggle_pause(attempt(1)), SubmitStatus::Accepted);
        assert_eq!(gate.toggle_pause(attempt(1)), SubmitStatus::NotReady);
        let (id, command) = driver
            .submitted
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let BackendCommand::SetPaused {
            request,
            paused: true,
        } = command
        else {
            panic!("pause command expected")
        };
        driver.send(BackendEvent::PauseObserved(PauseObservation {
            request: Some(request),
            paused: true,
        }));
        // A correlated property readback alone must not admit another command
        // while the owner's pause transaction still awaits its completion.
        driver.fence();
        assert!(!gate.poll().can_toggle_pause);
        assert_eq!(gate.toggle_pause(attempt(1)), SubmitStatus::NotReady);
        driver.send(BackendEvent::CommandReply {
            id: id.get(),
            error: 0,
        });
        let paused = until(&mut gate, |update| update.paused && update.can_toggle_pause);
        assert_eq!(paused.phase, GatePhase::Ready);
        assert_eq!(gate.toggle_pause(attempt(1)), SubmitStatus::Accepted);
        let (id, command) = driver
            .submitted
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        let BackendCommand::SetPaused {
            request,
            paused: false,
        } = command
        else {
            panic!("same-owner resume expected")
        };
        driver.send(BackendEvent::PauseObserved(PauseObservation {
            request: Some(request),
            paused: false,
        }));
        driver.send(BackendEvent::CommandReply {
            id: id.get(),
            error: 0,
        });
        let resumed = until(&mut gate, |update| {
            !update.paused && update.can_toggle_pause
        });
        assert_eq!(resumed.generation, 1);
        finish(&mut gate, &driver);
    }
    #[test]
    fn close_before_surface_waits_for_real_ack_and_ignores_stale_tokens() {
        let (mut gate, driver) = fixture(Config {
            hold_shutdown: true,
            ..Default::default()
        });
        gate.surface_ready(token(2));
        assert_eq!(gate.poll().phase, GatePhase::WaitingSurface);
        gate.request_close();
        driver
            .shutdown_started
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        gate.native_released(attempt(1));
        let waiting = gate.poll();
        assert_eq!(waiting.phase, GatePhase::Stopping);
        assert!(!waiting.release_native && !waiting.quit);
        driver.shutdown_release.send(()).unwrap();
        let release = until(&mut gate, |update| update.release_native);
        assert!(release.terminal_failure.is_none());
        driver
            .destroyed
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        gate.native_released(attempt(1));
        assert!(gate.poll().quit);
    }
    #[test]
    fn close_during_initialization_preserves_owner_barrier() {
        let (mut gate, driver) = fixture(Config {
            hold_initialize: true,
            hold_shutdown: true,
            ..Default::default()
        });
        gate.surface_ready(token(1));
        driver
            .initialized
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        gate.request_close();
        assert!(!gate.poll().release_native);
        driver.initialize_release.send(()).unwrap();
        driver
            .shutdown_started
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(!gate.poll().release_native);
        driver.shutdown_release.send(()).unwrap();
        until(&mut gate, |update| update.release_native);
        driver
            .destroyed
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        gate.native_released(attempt(1));
        assert!(gate.poll().quit);
    }
    #[test]
    fn terminal_beats_readiness_and_resume_and_survives_cleanup() {
        let (mut gate, driver) = fixture(Config {
            hold_shutdown: true,
            ..Default::default()
        });
        live(&mut gate, &driver);
        driver.send(BackendEvent::QueueOverflow);
        driver
            .shutdown_started
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(gate.toggle_pause(attempt(1)), SubmitStatus::Closing);
        assert_eq!(gate.poll().terminal_failure.unwrap().code, "event_overflow");
        assert!(!gate.poll().release_native);
        driver.shutdown_release.send(()).unwrap();
        until(&mut gate, |update| update.release_native);
        driver
            .destroyed
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        gate.native_released(attempt(1));
        let final_update = gate.poll();
        assert!(final_update.quit);
        assert_eq!(
            final_update.terminal_failure.unwrap().code,
            "event_overflow"
        );
    }
    #[test]
    fn surface_loss_retains_error_after_actual_destruction_ack() {
        let (mut gate, driver) = fixture(Config::default());
        live(&mut gate, &driver);
        gate.surface_lost(attempt(2));
        assert_eq!(gate.poll().phase, GatePhase::Ready);
        gate.surface_lost(attempt(1));
        gate.wait_for_owner_ack(attempt(1)).unwrap();
        let release = until(&mut gate, |update| update.release_native);
        assert_eq!(release.terminal_failure.unwrap().code, "surface_lost");
        driver
            .destroyed
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        gate.native_released(attempt(1));
        assert!(gate.poll().terminal_failure.is_some());
    }
    #[test]
    fn disconnected_ack_never_authorizes_surface_release() {
        let mut gate = QualificationGate::new();
        gate.endpoint = Some(
            OwnerEndpoint::spawn_with_backend(
                Generation::new(1).unwrap(),
                || -> crate::media::controller::test_support::FakeBackend {
                    panic!("injected owner factory panic")
                },
            )
            .unwrap(),
        );
        gate.phase = GatePhase::WaitingSurface;
        gate.surface_lost(attempt(1));
        assert!(gate.wait_for_owner_ack(attempt(1)).is_err());
        for _ in 0..4 {
            let update = gate.poll();
            assert!(!update.release_native && !update.quit);
        }
        gate.native_released(attempt(1));
        assert!(!gate.poll().quit);
    }
}
