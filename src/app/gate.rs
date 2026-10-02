//! Value-only lifecycle reducer. Native release requires a genuine owner ack.

use crate::capture::input::CaptureSelection;
#[cfg(test)]
use crate::media::controller::BackendEvent;
pub use crate::media::controller::{
    Generation, PlaybackIntent, SubmitStatus, SurfaceToken, X11WindowId,
};
use crate::media::controller::{MediaError, OwnerEndpoint, OwnerStopped, Snapshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseTarget {
    Session,
    Application,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatePhase {
    Idle,
    WaitingSurface,
    Opening,
    Ready,
    Stopping,
    Releasing,
    Failed,
    QuitReady,
}

/// Fixed native effects are consumed in create, release, quit order by C++.
#[derive(Debug)]
pub struct UiUpdate {
    pub changed: bool,
    pub phase: GatePhase,
    pub generation: Option<Generation>,
    pub failed: bool,
    pub failure_code: Option<&'static str>,
    pub diagnostic: String,
    pub paused: bool,
    pub ended: bool,
    pub create_native: bool,
    pub release_native: bool,
    pub quit: bool,
}

struct GateState {
    last_generation: u64,
    generation: Option<Generation>,
    cleanup: GatePhase,
    failure: Option<MediaError>,
    token: Option<SurfaceToken>,
    quit_requested: bool,
    paused: bool,
    ended: bool,
    report: String,
}
impl GateState {
    fn new(last_generation: u64) -> Self {
        Self {
            last_generation,
            generation: None,
            cleanup: GatePhase::Idle,
            failure: None,
            token: None,
            quit_requested: false,
            paused: false,
            ended: false,
            report: String::new(),
        }
    }
    fn phase(&self) -> GatePhase {
        if self.failure.is_some()
            && !matches!(self.cleanup, GatePhase::Releasing | GatePhase::QuitReady)
        {
            GatePhase::Failed
        } else {
            self.cleanup
        }
    }
    fn update(
        &self,
        changed: bool,
        create_native: bool,
        release_native: bool,
        quit: bool,
    ) -> UiUpdate {
        UiUpdate {
            changed,
            phase: self.phase(),
            generation: self.generation,
            failed: self.failure.is_some(),
            failure_code: self.failure.as_ref().map(|e| e.code),
            diagnostic: if changed {
                self.failure
                    .as_ref()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| self.report.clone())
            } else {
                String::new()
            },
            paused: self.paused,
            ended: self.ended,
            create_native,
            release_native,
            quit,
        }
    }
    fn unchanged(&self) -> UiUpdate {
        self.update(false, false, false, false)
    }
    fn matches(&self, generation: Generation) -> bool {
        self.generation == Some(generation)
    }

    fn open(&mut self) -> UiUpdate {
        if self.phase() != GatePhase::Idle {
            return self.unchanged();
        }
        let Some(next) = self
            .last_generation
            .checked_add(1)
            .and_then(Generation::new)
        else {
            self.failure = Some(MediaError::new(
                "generation_exhausted",
                "surface generation exhausted; process restart required",
            ));
            return self.update(true, false, false, false);
        };
        self.last_generation = next.get();
        self.generation = Some(next);
        self.cleanup = GatePhase::WaitingSurface;
        self.paused = false;
        self.ended = false;
        self.token = None;
        self.update(true, true, false, false)
    }

    fn surface_ready(&mut self, token: SurfaceToken) -> UiUpdate {
        if !self.matches(token.generation)
            || self.failure.is_some()
            || self.cleanup != GatePhase::WaitingSurface
            || self.token.is_some()
        {
            return self.unchanged();
        }
        self.token = Some(token);
        self.cleanup = GatePhase::Opening;
        self.update(true, false, false, false)
    }

    /// Closing handoff/intent rejections are consequences of an owner stop,
    /// not evidence for its cause. They must yield to a genuine owner/native
    /// failure, while the first independently observed failure stays latched.
    fn record_failure(&mut self, error: MediaError) -> bool {
        let secondary =
            |error: &MediaError| matches!(error.code, "surface_handoff" | "owner_unavailable");
        let replace = self
            .failure
            .as_ref()
            .is_none_or(|current| secondary(current) && !secondary(&error));
        if replace {
            self.failure = Some(error);
        }
        replace
    }

    fn fail(&mut self, error: MediaError) -> UiUpdate {
        let changed = self.record_failure(error)
            || self.token.is_some()
            || !matches!(self.cleanup, GatePhase::Stopping | GatePhase::Releasing);
        self.token = None;
        if self.generation.is_some() && self.cleanup != GatePhase::Releasing {
            self.cleanup = GatePhase::Stopping;
        }
        self.update(changed, false, false, false)
    }

    fn surface_lost(&mut self, generation: Generation) -> UiUpdate {
        if !self.matches(generation) || self.cleanup == GatePhase::Releasing {
            return self.unchanged();
        }
        self.fail(MediaError::new(
            "surface_lost",
            "native surface destroyed before owner completion; generation revoked",
        ))
    }

    fn close(&mut self, generation: Generation, target: CloseTarget) -> UiUpdate {
        if !self.matches(generation) {
            return self.unchanged();
        }
        let escalating = target == CloseTarget::Application && !self.quit_requested;
        self.quit_requested |= target == CloseTarget::Application;
        if matches!(self.cleanup, GatePhase::Stopping | GatePhase::Releasing) {
            return self.update(escalating, false, false, false);
        }
        self.cleanup = GatePhase::Stopping;
        self.token = None;
        self.update(true, false, false, false)
    }

    fn request_quit(&mut self) -> UiUpdate {
        if self.cleanup == GatePhase::QuitReady {
            return self.unchanged();
        }
        if let Some(generation) = self.generation {
            return self.close(generation, CloseTarget::Application);
        }
        self.quit_requested = true;
        self.cleanup = GatePhase::QuitReady;
        self.update(true, false, false, true)
    }

    fn owner_snapshot(&mut self, snapshot: Snapshot) -> UiUpdate {
        if !self.matches(snapshot.generation)
            || matches!(self.cleanup, GatePhase::Releasing | GatePhase::QuitReady)
        {
            return self.unchanged();
        }
        if let Some(error) = snapshot.failure {
            return self.fail(error);
        }
        if self.cleanup == GatePhase::Stopping || self.failure.is_some() {
            return self.unchanged();
        }
        let mut changed = self.paused != snapshot.paused || self.ended != snapshot.ended;
        self.paused = snapshot.paused;
        self.ended = snapshot.ended;
        if let Some(session) = snapshot.session {
            let report = session.summary();
            changed |= self.report != report;
            self.report = report;
        }
        if snapshot.initialized
            && snapshot.playback_started
            && self.cleanup == GatePhase::Opening
            && self.token.is_some()
        {
            self.cleanup = GatePhase::Ready;
            changed = true;
        }
        self.update(changed, false, false, false)
    }

    fn owner_stopped(&mut self, stopped: OwnerStopped) -> UiUpdate {
        if !self.matches(stopped.generation)
            || matches!(self.cleanup, GatePhase::Releasing | GatePhase::QuitReady)
        {
            return self.unchanged();
        }
        if let Err(error) = stopped.outcome {
            self.record_failure(error);
        }
        self.token = None;
        self.cleanup = GatePhase::Releasing;
        self.update(true, false, true, false)
    }

    fn native_released(&mut self, generation: Generation) -> UiUpdate {
        if !self.matches(generation) || self.cleanup != GatePhase::Releasing {
            return self.unchanged();
        }
        self.generation = None;
        self.token = None;
        self.cleanup = if self.quit_requested {
            GatePhase::QuitReady
        } else {
            GatePhase::Idle
        };
        self.update(true, false, false, self.quit_requested)
    }
}

/// Qt calls this only on its GUI thread. No Qt pointer, handle or callback is stored.
pub struct GateCoordinator {
    state: GateState,
    endpoint: Option<OwnerEndpoint>,
    spawn: Box<dyn FnMut(Generation) -> Result<OwnerEndpoint, MediaError>>,
    pending_update: bool,
    capture_selected: bool,
}
impl GateCoordinator {
    pub fn new(media_prefix: String, selection: Option<CaptureSelection>) -> Self {
        let capture_selected = selection.is_some();
        let report = selection.as_ref().map(|selection| format!("Requested: {}\nOpen capture to start.", selection.requested()))
            .unwrap_or_else(|| "No capture selected. Start with --capture-node, --capture-fourcc, --capture-size and --capture-rate.".into());
        let mut coordinator = Self::with_spawner(move |generation| {
            let selection = selection.clone().ok_or_else(|| {
                MediaError::new("capture_selection", "no explicit capture selection")
            })?;
            OwnerEndpoint::spawn(generation, media_prefix.clone(), selection)
        });
        coordinator.capture_selected = capture_selected;
        coordinator.state.report = report;
        coordinator.pending_update = true;
        coordinator
    }
    pub(crate) fn capture_selected(&self) -> bool {
        self.capture_selected
    }
    fn with_spawner(
        spawn: impl FnMut(Generation) -> Result<OwnerEndpoint, MediaError> + 'static,
    ) -> Self {
        Self {
            state: GateState::new(0),
            endpoint: None,
            spawn: Box::new(spawn),
            pending_update: false,
            capture_selected: true,
        }
    }
    fn synchronize_stop(&self) {
        if self.state.cleanup == GatePhase::Stopping
            && let (Some(endpoint), Some(generation)) = (&self.endpoint, self.state.generation)
        {
            endpoint.stop(generation, self.state.failure.clone());
        }
    }

    pub fn open(&mut self) -> UiUpdate {
        if !self.capture_selected {
            return self.state.update(true, false, false, false);
        }
        let update = self.state.open();
        if !update.create_native {
            return update;
        }
        let generation = update.generation.expect("opening allocates generation");
        match (self.spawn)(generation) {
            Ok(endpoint) => {
                self.endpoint = Some(endpoint);
                update
            }
            Err(error) => {
                // No worker or native host exists, so neither an ack nor release is invented.
                self.state.generation = None;
                self.state.cleanup = GatePhase::Idle;
                self.state.fail(error)
            }
        }
    }

    pub fn surface_ready(&mut self, token: SurfaceToken) -> UiUpdate {
        let update = self.state.surface_ready(token);
        if !update.changed {
            return update;
        }
        match self
            .endpoint
            .as_ref()
            .map(|endpoint| endpoint.attach(token))
        {
            Some(SubmitStatus::Accepted) => update,
            _ => {
                let update = self.state.fail(MediaError::new(
                    "surface_handoff",
                    "owner rejected native surface handoff",
                ));
                self.synchronize_stop();
                update
            }
        }
    }

    pub fn surface_lost(&mut self, generation: Generation) -> UiUpdate {
        let update = self.state.surface_lost(generation);
        if update.changed {
            self.synchronize_stop();
        }
        update
    }

    /// Wait inside Qt's pre-destruction callback; leave the ack and release for poll.
    pub(crate) fn wait_for_owner_ack(&mut self, generation: Generation) -> Result<(), MediaError> {
        match self.endpoint.as_mut() {
            Some(endpoint)
                if self.state.matches(generation)
                    && endpoint.generation() == generation
                    && self.state.failure.is_some()
                    && self.state.cleanup == GatePhase::Stopping =>
            {
                endpoint.wait_for_ack()
            }
            _ => Err(MediaError::new(
                "surface_loss_barrier",
                "surface-loss barrier requires the active failed generation and a live owner endpoint",
            )),
        }
    }

    pub fn submit(&mut self, generation: Generation, intent: PlaybackIntent) -> SubmitStatus {
        if !self.state.matches(generation) {
            return SubmitStatus::StaleGeneration;
        }
        if self.state.failure.is_some()
            || matches!(
                self.state.cleanup,
                GatePhase::Stopping | GatePhase::Releasing | GatePhase::QuitReady
            )
        {
            return SubmitStatus::Closing;
        }
        if self.state.cleanup != GatePhase::Ready || self.state.token.is_none() {
            return SubmitStatus::NotReady;
        }
        let status = self
            .endpoint
            .as_ref()
            .map(|endpoint| endpoint.submit(generation, intent))
            .unwrap_or(SubmitStatus::Closing);
        if status == SubmitStatus::CapacityExceeded {
            self.state.fail(MediaError::new(
                "command_overflow",
                "64-command owner queue full; session stopped",
            ));
            self.pending_update = true;
            self.synchronize_stop();
        } else if status == SubmitStatus::Closing {
            self.state.fail(MediaError::new(
                "owner_unavailable",
                "owner no longer accepts playback commands",
            ));
            self.pending_update = true;
            self.synchronize_stop();
        }
        status
    }

    pub fn close(&mut self, generation: Generation, target: CloseTarget) -> UiUpdate {
        let update = self.state.close(generation, target);
        if update.changed {
            self.synchronize_stop();
        }
        update
    }
    pub fn request_quit(&mut self) -> UiUpdate {
        let update = self.state.request_quit();
        if update.changed {
            self.synchronize_stop();
        }
        update
    }
    pub fn native_released(&mut self, generation: Generation) -> UiUpdate {
        let update = self.state.native_released(generation);
        if update.changed {
            self.endpoint = None;
        }
        update
    }
    pub fn poll(&mut self) -> UiUpdate {
        let Some(endpoint) = self.endpoint.as_mut() else {
            if self.pending_update {
                self.pending_update = false;
                return self.state.update(true, false, false, false);
            }
            return self.state.unchanged();
        };
        // Terminal ack must beat any coalesced FileLoaded/Ready snapshot.
        match endpoint.take_stopped() {
            Ok(Some(stopped)) => {
                self.pending_update = false;
                tracing::info!(generation = stopped.generation.get(), "ui_stop_ack");
                return self.state.owner_stopped(stopped);
            }
            Err(error) => {
                let update = self.state.fail(error);
                let update = if self.pending_update {
                    self.pending_update = false;
                    self.state.update(true, false, false, false)
                } else {
                    update
                };
                if update.changed {
                    self.synchronize_stop();
                }
                return update;
            }
            Ok(None) => {}
        }
        if self.pending_update {
            self.pending_update = false;
            return self.state.update(true, false, false, false);
        }
        let snapshot = endpoint.take_snapshot();
        let update = snapshot
            .map(|snapshot| self.state.owner_snapshot(snapshot))
            .unwrap_or_else(|| self.state.unchanged());
        if update.changed {
            self.synchronize_stop();
        }
        update
    }
    pub(crate) fn unchanged(&self) -> UiUpdate {
        self.state.unchanged()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::controller::test_support::{Config, Driver};
    use std::sync::mpsc;

    #[derive(Default)]
    struct FakeHost {
        live: Option<Generation>,
        released: Vec<Generation>,
        quit: bool,
    }
    impl FakeHost {
        fn apply(&mut self, update: &UiUpdate) {
            if update.create_native {
                assert!(self.live.is_none());
                self.live = update.generation;
            }
            if update.release_native {
                assert_eq!(self.live, update.generation);
                self.released.push(self.live.take().unwrap());
            }
            if update.quit {
                assert!(self.live.is_none());
                self.quit = true;
            }
        }
    }
    fn token(generation: Generation) -> SurfaceToken {
        SurfaceToken {
            generation,
            xid: X11WindowId::new(71).unwrap(),
        }
    }
    fn snapshot(generation: Generation, ready: bool, ended: bool) -> Snapshot {
        Snapshot {
            generation,
            initialized: true,
            file_loaded: ready,
            playback_started: ready,
            session: None,
            paused: false,
            ended,
            failure: None,
        }
    }
    fn opened() -> (GateState, FakeHost, Generation) {
        let mut state = GateState::new(0);
        let mut host = FakeHost::default();
        let update = state.open();
        let generation = update.generation.unwrap();
        host.apply(&update);
        (state, host, generation)
    }
    fn coordinator(config: Config) -> (GateCoordinator, mpsc::Receiver<Driver>) {
        let (tx, rx) = mpsc::channel();
        let mut config = Some(config);
        let gate = GateCoordinator::with_spawner(move |generation| {
            let (driver, backend) = Driver::pair(config.take().unwrap_or_default());
            tx.send(driver).unwrap();
            OwnerEndpoint::spawn_with_backend(generation, move || backend)
        });
        (gate, rx)
    }
    fn await_owner(gate: &mut GateCoordinator) -> UiUpdate {
        gate.endpoint.as_mut().unwrap().wait_for_ack().unwrap();
        gate.poll()
    }

    #[test]
    fn handoff_occurs_once_and_duplicate_or_stale_surface_never_reattaches() {
        let (mut gate, drivers) = coordinator(Config::default());
        let g = gate.open().generation.unwrap();
        let driver = drivers.recv().unwrap();
        assert_eq!(gate.surface_ready(token(g)).phase, GatePhase::Opening);
        assert_eq!(driver.initialized.recv().unwrap(), token(g));
        gate.surface_ready(token(g));
        gate.surface_ready(token(Generation::new(g.get() + 1).unwrap()));
        assert!(driver.initialized.try_recv().is_err());
        assert_eq!(
            gate.submit(g, PlaybackIntent::TogglePause),
            SubmitStatus::NotReady
        );
        gate.close(g, CloseTarget::Session);
        await_owner(&mut gate);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn end_file_shutdown_and_incomplete_cleanup_cannot_release_native_parent() {
        let (mut gate, drivers) = coordinator(Config {
            hold_shutdown: true,
            ..Config::default()
        });
        let mut host = FakeHost::default();
        let update = gate.open();
        let g = update.generation.unwrap();
        host.apply(&update);
        let driver = drivers.recv().unwrap();
        gate.surface_ready(token(g));
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        driver.send(BackendEvent::PlaybackRestart);
        driver.send(BackendEvent::EndFile {
            reason: 0,
            error: 0,
        });
        driver.fence();
        host.apply(&gate.poll());
        assert_eq!(host.live, Some(g));
        let close = gate.close(g, CloseTarget::Application);
        host.apply(&close);
        host.apply(&gate.poll());
        assert_eq!(host.live, Some(g));
        assert!(!host.quit);
        driver.shutdown_release.send(()).unwrap();
        let release = await_owner(&mut gate);
        assert_eq!(release.phase, GatePhase::Releasing);
        host.apply(&release);
        assert!(!host.quit);
        host.apply(&gate.native_released(g));
        assert!(host.quit);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn close_before_publication_rejects_delayed_token_then_next_cycle_uses_greater_generation() {
        let (mut gate, drivers) = coordinator(Config::default());
        let first = gate.open().generation.unwrap();
        let first_driver = drivers.recv().unwrap();
        gate.close(first, CloseTarget::Session);
        gate.surface_ready(token(first));
        await_owner(&mut gate);
        first_driver.destroyed.recv().unwrap();
        assert!(first_driver.initialized.try_recv().is_err());
        assert_eq!(gate.native_released(first).phase, GatePhase::Idle);
        let second = gate.open().generation.unwrap();
        assert!(second.get() > first.get());
        let driver = drivers.recv().unwrap();
        gate.surface_ready(token(second));
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        assert_eq!(gate.poll().phase, GatePhase::Ready);
        gate.close(second, CloseTarget::Session);
        await_owner(&mut gate);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn close_during_initialization_and_pending_load_never_revives_late_ready() {
        for held in [true, false] {
            let (mut gate, drivers) = coordinator(Config {
                hold_initialize: held,
                ..Config::default()
            });
            let g = gate.open().generation.unwrap();
            let driver = drivers.recv().unwrap();
            gate.surface_ready(token(g));
            driver.initialized.recv().unwrap();
            if !held {
                driver.submitted.recv().unwrap();
            }
            gate.close(g, CloseTarget::Session);
            if held {
                driver.initialize_release.send(()).unwrap();
            }
            let release = await_owner(&mut gate);
            assert!(release.release_native);
            assert_ne!(release.phase, GatePhase::Ready);
            if held {
                assert!(driver.submitted.try_recv().is_err());
            }
            driver.destroyed.recv().unwrap();
        }
    }

    #[test]
    fn failed_open_destroys_partial_handle_once_and_failure_stays_latched() {
        for handle in [false, true] {
            let (mut gate, drivers) = coordinator(Config {
                creates_handle: handle,
                initialization_error: Some(MediaError::new(
                    "initialization",
                    "injected open failure",
                )),
                hold_shutdown: true,
                ..Config::default()
            });
            let g = gate.open().generation.unwrap();
            let driver = drivers.recv().unwrap();
            gate.surface_ready(token(g));
            driver.initialized.recv().unwrap();
            assert!(!gate.poll().release_native);
            driver.shutdown_release.send(()).unwrap();
            let released = await_owner(&mut gate);
            assert!(released.failed && released.release_native);
            assert_eq!(driver.destroyed.recv().unwrap(), handle);
            assert!(driver.destroyed.try_recv().is_err());
            assert_eq!(gate.native_released(g).phase, GatePhase::Failed);
            assert!(!gate.open().create_native);
        }
    }

    #[test]
    fn spawn_failure_has_no_host_and_no_fabricated_stopped_ack() {
        let mut gate = GateCoordinator::with_spawner(|_| {
            Err(MediaError::new("spawn", "injected spawn failure"))
        });
        let update = gate.open();
        assert!(update.failed);
        assert!(!update.create_native && !update.release_native);
        assert_eq!(update.phase, GatePhase::Failed);
        assert!(gate.endpoint.is_none());
    }

    #[test]
    fn stale_generation_cannot_modify_new_token_effects_or_cancellation_even_with_reused_xid() {
        let (mut state, mut host, first) = opened();
        state.surface_ready(token(first));
        state.close(first, CloseTarget::Session);
        host.apply(&state.owner_stopped(OwnerStopped {
            generation: first,
            outcome: Ok(()),
        }));
        state.native_released(first);
        let new = state.open();
        let second = new.generation.unwrap();
        host.apply(&new);
        state.surface_ready(token(second));
        state.owner_snapshot(snapshot(second, true, false));
        for update in [
            state.surface_ready(token(first)),
            state.surface_lost(first),
            state.close(first, CloseTarget::Application),
            state.owner_stopped(OwnerStopped {
                generation: first,
                outcome: Err(MediaError::new("old", "old failure")),
            }),
            state.native_released(first),
        ] {
            assert!(!update.create_native && !update.release_native && !update.quit);
        }
        assert_eq!(state.phase(), GatePhase::Ready);
        assert_eq!(state.token, Some(token(second)));
        assert!(!state.quit_requested);
    }

    #[test]
    fn forced_loss_is_immediate_terminal_and_planned_loss_allowed_only_after_ack() {
        for during_stop in [false, true] {
            let (mut state, _, g) = opened();
            state.surface_ready(token(g));
            state.owner_snapshot(snapshot(g, true, false));
            if during_stop {
                state.close(g, CloseTarget::Session);
            }
            let loss = state.surface_lost(g);
            assert!(loss.failed);
            assert_eq!(loss.phase, GatePhase::Failed);
            assert!(state.token.is_none());
            assert!(!state.surface_ready(token(g)).create_native);
            let release = state.owner_stopped(OwnerStopped {
                generation: g,
                outcome: Ok(()),
            });
            assert!(release.release_native && release.failed);
            assert_eq!(state.native_released(g).phase, GatePhase::Failed);
        }
        let (mut state, _, g) = opened();
        state.close(g, CloseTarget::Session);
        state.owner_stopped(OwnerStopped {
            generation: g,
            outcome: Ok(()),
        });
        assert!(!state.surface_lost(g).failed);
        assert_eq!(state.native_released(g).phase, GatePhase::Idle);
    }

    #[test]
    fn generation_exhaustion_does_not_reuse_and_close_is_idempotent_with_quit_escalation() {
        let mut exhausted = GateState::new(u64::MAX);
        let failed = exhausted.open();
        assert!(failed.failed && !failed.create_native);
        assert_eq!(failed.generation, None);
        let (mut state, _, g) = opened();
        state.close(g, CloseTarget::Session);
        assert!(!state.close(g, CloseTarget::Session).changed);
        state.close(g, CloseTarget::Application);
        assert!(state.quit_requested);
        assert!(
            !state
                .owner_stopped(OwnerStopped {
                    generation: g,
                    outcome: Ok(())
                })
                .quit
        );
        assert!(state.native_released(g).quit);
    }

    #[test]
    fn ack_precedes_coalesced_ready_and_prevents_resurrection() {
        let (mut gate, drivers) = coordinator(Config::default());
        let g = gate.open().generation.unwrap();
        let driver = drivers.recv().unwrap();
        gate.surface_ready(token(g));
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        gate.close(g, CloseTarget::Session);
        let update = await_owner(&mut gate);
        assert_eq!(update.phase, GatePhase::Releasing);
        assert!(update.release_native);
        assert_ne!(gate.poll().phase, GatePhase::Ready);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn eof_snapshot_retains_ready_and_session_until_explicit_close() {
        let (mut state, mut host, g) = opened();
        state.surface_ready(token(g));
        let update = state.owner_snapshot(snapshot(g, true, true));
        assert_eq!(update.phase, GatePhase::Ready);
        assert!(update.ended && !update.release_native);
        host.apply(&update);
        assert_eq!(host.live, Some(g));
    }

    #[test]
    fn xid_and_generation_boundaries_reject_zero_and_xid_truncation() {
        assert!(Generation::new(0).is_none());
        assert!(X11WindowId::new(0).is_none());
        assert!(X11WindowId::new(u64::from(u32::MAX) + 1).is_none());
        assert_eq!(
            X11WindowId::new(u64::from(u32::MAX)).unwrap().get(),
            u32::MAX
        );
    }

    #[test]
    fn command_overload_is_visible_before_owner_destruction_ack() {
        let (mut gate, drivers) = coordinator(Config {
            hold_shutdown: true,
            ..Config::default()
        });
        let g = gate.open().generation.unwrap();
        let driver = drivers.recv().unwrap();
        gate.surface_ready(token(g));
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        assert_eq!(gate.poll().phase, GatePhase::Ready);
        for _ in 0..64 {
            assert_eq!(
                gate.submit(g, PlaybackIntent::TogglePause),
                SubmitStatus::Accepted
            );
        }
        assert_eq!(
            gate.submit(g, PlaybackIntent::TogglePause),
            SubmitStatus::CapacityExceeded
        );
        let update = gate.poll();
        assert!(update.changed && update.failed && !update.release_native);
        assert_eq!(update.phase, GatePhase::Failed);
        assert_eq!(update.failure_code, Some("command_overflow"));
        assert_eq!(
            gate.submit(g, PlaybackIntent::TogglePause),
            SubmitStatus::Closing
        );
        driver.shutdown_release.send(()).unwrap();
        assert!(await_owner(&mut gate).release_native);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn surface_loss_barrier_revokes_during_held_shutdown_and_only_poll_releases() {
        for during_stop in [false, true] {
            let (mut gate, drivers) = coordinator(Config {
                creates_handle: true,
                hold_shutdown: true,
                ..Config::default()
            });
            let mut host = FakeHost::default();
            let opening = gate.open();
            let g = opening.generation.unwrap();
            host.apply(&opening);
            let driver = drivers.recv().unwrap();
            gate.surface_ready(token(g));
            driver.initialized.recv().unwrap();
            driver.submitted.recv().unwrap();
            driver.send(BackendEvent::PlaybackRestart);
            driver.fence();
            assert_eq!(gate.poll().phase, GatePhase::Ready);
            if during_stop {
                host.apply(&gate.close(g, CloseTarget::Session));
            }

            let loss = gate.surface_lost(g);
            assert!(loss.changed && loss.failed);
            assert_eq!(loss.phase, GatePhase::Failed);
            assert_eq!(loss.failure_code, Some("surface_lost"));
            assert!(!loss.release_native && !loss.quit);
            host.apply(&loss);
            assert!(gate.state.token.is_none());
            assert_eq!(gate.state.cleanup, GatePhase::Stopping);
            assert_eq!(
                gate.submit(g, PlaybackIntent::TogglePause),
                SubmitStatus::Closing
            );
            assert_eq!(
                gate.endpoint.as_ref().unwrap().attach(token(g)),
                SubmitStatus::Closing
            );
            assert!(!gate.surface_ready(token(g)).changed);
            assert!(!gate.state.owner_snapshot(snapshot(g, true, false)).changed);

            let stale = Generation::new(g.get() + 1).unwrap();
            assert_eq!(
                gate.wait_for_owner_ack(stale).unwrap_err(),
                MediaError::new(
                    "surface_loss_barrier",
                    "surface-loss barrier requires the active failed generation and a live owner endpoint",
                )
            );
            let pending = gate.poll();
            assert!(pending.failed && !pending.release_native);
            assert_eq!(pending.phase, GatePhase::Failed);
            assert!(
                gate.endpoint
                    .as_mut()
                    .unwrap()
                    .take_stopped()
                    .unwrap()
                    .is_none()
            );
            assert!(driver.destroyed.try_recv().is_err());
            assert_eq!(host.live, Some(g));
            assert!(host.released.is_empty());

            driver.shutdown_release.send(()).unwrap();
            gate.wait_for_owner_ack(g).unwrap();
            assert!(driver.destroyed.try_recv().unwrap());
            gate.wait_for_owner_ack(g).unwrap();
            assert_eq!(gate.state.cleanup, GatePhase::Stopping);
            assert_eq!(gate.unchanged().phase, GatePhase::Failed);
            assert!(gate.state.token.is_none());
            assert_eq!(host.live, Some(g));
            assert!(host.released.is_empty());

            let release = gate.poll();
            assert!(release.changed && release.failed && release.release_native);
            assert_eq!(release.phase, GatePhase::Releasing);
            assert_eq!(release.failure_code, Some("surface_lost"));
            host.apply(&release);
            assert_eq!(host.released, vec![g]);
            let duplicate = gate.poll();
            assert!(!duplicate.changed && !duplicate.release_native);
            host.apply(&duplicate);
            assert_eq!(host.released, vec![g]);
            assert_eq!(gate.native_released(g).phase, GatePhase::Failed);
            assert!(!gate.open().create_native);
        }
    }

    #[test]
    fn surface_loss_barrier_rejects_pre_stop_and_stale_generation_without_mutation() {
        let (mut gate, drivers) = coordinator(Config::default());
        let invalid = MediaError::new(
            "surface_loss_barrier",
            "surface-loss barrier requires the active failed generation and a live owner endpoint",
        );
        let first = gate.open().generation.unwrap();
        let first_driver = drivers.recv().unwrap();
        gate.close(first, CloseTarget::Session);
        assert!(await_owner(&mut gate).release_native);
        first_driver.destroyed.recv().unwrap();
        assert_eq!(gate.native_released(first).phase, GatePhase::Idle);
        assert_eq!(gate.wait_for_owner_ack(first).unwrap_err(), invalid);
        assert_eq!(gate.unchanged().phase, GatePhase::Idle);

        let second = gate.open().generation.unwrap();
        assert!(second.get() > first.get());
        let driver = drivers.recv().unwrap();
        gate.surface_ready(token(second));
        driver.initialized.recv().unwrap();
        let (load, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        assert_eq!(gate.poll().phase, GatePhase::Ready);
        for generation in [first, second] {
            assert_eq!(gate.wait_for_owner_ack(generation).unwrap_err(), invalid);
            assert_eq!(gate.state.generation, Some(second));
            assert_eq!(gate.state.token, Some(token(second)));
            assert_eq!(gate.state.cleanup, GatePhase::Ready);
            assert!(gate.state.failure.is_none());
            assert!(!gate.poll().changed);
        }
        assert_eq!(
            gate.submit(second, PlaybackIntent::TogglePause),
            SubmitStatus::Accepted
        );
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        assert_eq!(
            driver.submitted.recv().unwrap().1,
            crate::media::controller::BackendCommand::TogglePause
        );
        gate.close(second, CloseTarget::Session);
        assert_eq!(gate.wait_for_owner_ack(second).unwrap_err(), invalid);
        assert_eq!(gate.state.cleanup, GatePhase::Stopping);
        assert!(gate.state.failure.is_none());
        assert!(await_owner(&mut gate).release_native);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn surface_loss_barrier_disconnected_owner_cannot_release_native() {
        let mut gate = GateCoordinator::with_spawner(|generation| {
            OwnerEndpoint::spawn_with_backend(
                generation,
                || -> crate::media::controller::test_support::FakeBackend {
                    panic!("injected owner factory panic");
                },
            )
        });
        let mut host = FakeHost::default();
        let opening = gate.open();
        let g = opening.generation.unwrap();
        host.apply(&opening);
        let loss = gate.surface_lost(g);
        assert!(loss.failed && !loss.release_native);
        host.apply(&loss);
        let expected = MediaError::new(
            "owner_disconnect",
            "owner exited without destruction completion acknowledgment",
        );
        assert_eq!(gate.wait_for_owner_ack(g).unwrap_err(), expected);
        assert_eq!(gate.wait_for_owner_ack(g).unwrap_err(), expected);
        assert_eq!(gate.state.cleanup, GatePhase::Stopping);
        assert!(
            gate.endpoint
                .as_mut()
                .unwrap()
                .take_stopped()
                .unwrap()
                .is_none()
        );
        let update = gate.poll();
        assert!(update.failed && !update.release_native && !update.quit);
        assert_eq!(update.phase, GatePhase::Failed);
        assert_eq!(update.failure_code, Some("surface_lost"));
        host.apply(&update);
        assert_eq!(host.live, Some(g));
        assert!(host.released.is_empty());
    }
    #[test]
    fn file_loaded_headers_never_make_gate_ready_and_no_selection_creates_nothing() {
        let mut empty = GateCoordinator::new(String::new(), None);
        let notice = empty.poll();
        assert_eq!(notice.phase, GatePhase::Idle);
        assert!(notice.diagnostic.contains("--capture-node"));
        assert!(!empty.open().create_native);
        assert!(empty.endpoint.is_none());

        let (mut gate, drivers) = coordinator(Config::default());
        let generation = gate.open().generation.unwrap();
        let driver = drivers.recv().unwrap();
        gate.surface_ready(token(generation));
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        driver.send(BackendEvent::FileLoaded);
        driver.fence();
        assert_eq!(gate.poll().phase, GatePhase::Opening);
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        assert_eq!(gate.poll().phase, GatePhase::Ready);
        gate.close(generation, CloseTarget::Session);
        assert!(await_owner(&mut gate).release_native);
        driver.destroyed.recv().unwrap();
    }

    fn capture_race_coordinator(
        advertised_rate: u32,
        hold_shutdown: bool,
    ) -> (
        GateCoordinator,
        mpsc::Receiver<Driver>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        crate::media::session::RequestedFacts,
    ) {
        use crate::{
            capture::linux,
            domain::capture::{CaptureMode, CapturedFourCc, FrameRate, FrameSize},
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(2560, 1440).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        let initial = linux::session_fixture(&["/dev/video0"], mode);
        let selection =
            CaptureSelection::from_snapshot(&initial, std::path::Path::new("/dev/video0"), mode)
                .unwrap();
        let requested = selection.requested();
        let factories = Arc::new(AtomicUsize::new(0));
        let factory_count = Arc::clone(&factories);
        let (tx, rx) = mpsc::channel();
        let gate = GateCoordinator::with_spawner(move |generation| {
            let (driver, backend) = Driver::pair(Config {
                hold_shutdown,
                ..Config::default()
            });
            tx.send(driver).unwrap();
            let selection = selection.clone();
            let requested = selection.requested();
            let factory_count = Arc::clone(&factory_count);
            OwnerEndpoint::spawn_capture_with_backend(
                generation,
                requested,
                move || {
                    let advertised = CaptureMode {
                        rate: FrameRate::new(advertised_rate, 1).unwrap(),
                        ..mode
                    };
                    selection
                        .validate_snapshot(&linux::session_fixture(&["/dev/video0"], advertised))
                },
                move |_| {
                    factory_count.fetch_add(1, Ordering::SeqCst);
                    backend
                },
            )
        });
        (gate, rx, factories, requested)
    }

    #[test]
    fn authoritative_prevalidation_failure_survives_delayed_surface_handoff_rejection() {
        use crate::media::session::{Cause, Stage};
        let (mut gate, drivers, factories, requested) = capture_race_coordinator(30, false);
        let mut host = FakeHost::default();
        let opening = gate.open();
        let generation = opening.generation.unwrap();
        host.apply(&opening);
        let driver = drivers.recv().unwrap();
        gate.endpoint.as_mut().unwrap().wait_for_ack().unwrap();
        let rejected = gate.surface_ready(token(generation));
        assert!(rejected.failed && !rejected.release_native);
        host.apply(&rejected);
        let release = gate.poll();
        let error = gate
            .state
            .failure
            .as_ref()
            .unwrap()
            .session
            .as_ref()
            .unwrap();
        assert_eq!(error.requested, requested);
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Prevalidation, Cause::RequestedModeRefused)
        );
        assert_eq!(release.failure_code, Some("capture_session"));
        assert!(release.diagnostic.contains("2560x1440"));
        assert!(release.failed && release.release_native);
        host.apply(&release);
        assert_eq!(factories.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(driver.initialized.try_recv().is_err());
        assert!(driver.submitted.try_recv().is_err());
        assert!(!gate.poll().release_native);
        host.apply(&gate.native_released(generation));
        assert!(!gate.poll().release_native);
        assert_eq!(host.released, vec![generation]);
    }

    #[test]
    fn authoritative_verification_failure_survives_pause_rejection_before_destruction_ack() {
        use crate::{
            domain::capture::FrameSize,
            media::session::{Cause, Observation, ObservedFacts, Source, Stage},
        };
        let (mut gate, drivers, factories, requested) = capture_race_coordinator(60, true);
        let mut host = FakeHost::default();
        let opening = gate.open();
        let generation = opening.generation.unwrap();
        host.apply(&opening);
        let driver = drivers.recv().unwrap();
        gate.surface_ready(token(generation));
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        assert_eq!(gate.poll().phase, GatePhase::Ready);
        driver.set_observed(ObservedFacts {
            decoded_size: Some(Observation {
                value: FrameSize::new(1920, 1080).unwrap(),
                source: Source::MpvDecodedParams,
            }),
            ..ObservedFacts::default()
        });
        driver.send(BackendEvent::VideoReconfig);
        driver.shutdown_started.recv().unwrap();
        assert_eq!(
            gate.submit(generation, PlaybackIntent::TogglePause),
            SubmitStatus::Closing
        );
        let pending = gate.poll();
        let failure = gate.poll();
        // Always release the held owner before assertions can unwind this test.
        driver.shutdown_release.send(()).unwrap();
        gate.endpoint.as_mut().unwrap().wait_for_ack().unwrap();
        let error = gate
            .state
            .failure
            .as_ref()
            .unwrap()
            .session
            .as_ref()
            .unwrap();
        assert_eq!(error.requested, requested);
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Verification, Cause::RequestedModeRefused)
        );
        assert_eq!(failure.failure_code, Some("capture_session"));
        assert!(failure.failed && !failure.release_native);
        assert!(!pending.release_native);
        assert_eq!(host.live, Some(generation));
        let release = gate.poll();
        assert_eq!(release.failure_code, Some("capture_session"));
        assert!(release.release_native);
        host.apply(&release);
        driver.destroyed.recv().unwrap();
        assert_eq!(factories.load(std::sync::atomic::Ordering::SeqCst), 1);
        host.apply(&gate.native_released(generation));
        assert!(!gate.poll().release_native);
        assert_eq!(host.released, vec![generation]);
    }

    #[test]
    fn genuine_surface_loss_supersedes_secondary_pause_rejection_and_survives_owner_ack() {
        let (mut gate, drivers, _, _) = capture_race_coordinator(60, true);
        let generation = gate.open().generation.unwrap();
        let driver = drivers.recv().unwrap();
        gate.surface_ready(token(generation));
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        assert_eq!(gate.poll().phase, GatePhase::Ready);
        driver.send(BackendEvent::EndFile {
            reason: 4,
            error: -13,
        });
        driver.shutdown_started.recv().unwrap();
        assert_eq!(
            gate.submit(generation, PlaybackIntent::TogglePause),
            SubmitStatus::Closing
        );
        let loss = gate.surface_lost(generation);
        driver.shutdown_release.send(()).unwrap();
        gate.endpoint.as_mut().unwrap().wait_for_ack().unwrap();
        assert_eq!(loss.failure_code, Some("surface_lost"));
        assert!(loss.failed && !loss.release_native);
        let release = gate.poll();
        assert_eq!(release.failure_code, Some("surface_lost"));
        assert!(release.failed && release.release_native);
        driver.destroyed.recv().unwrap();
        gate.native_released(generation);
        assert!(!gate.poll().release_native);
    }
}
