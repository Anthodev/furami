//! Value-only native lifetime reducer. Product policy lives in ApplyCoordinator.

use crate::domain::{failure::ApplyFailure, state::AttemptId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GatePhase {
    Idle,
    WaitingSurface,
    Opening,
    Ready,
    Stopping,
    Releasing,
    Failed,
    QuitReady,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GateUpdate {
    pub changed: bool,
    pub phase: GatePhase,
    pub attempt: Option<AttemptId>,
    pub create_native: bool,
    pub release_native: bool,
}

pub(crate) struct GateState {
    last_attempt: Option<AttemptId>,
    attempt: Option<AttemptId>,
    cleanup: GatePhase,
    failure: Option<ApplyFailure>,
    surface: bool,
    blocked: bool,
}

impl GateState {
    pub(crate) fn new() -> Self {
        Self {
            last_attempt: None,
            attempt: None,
            cleanup: GatePhase::Idle,
            failure: None,
            surface: false,
            blocked: false,
        }
    }
    pub(crate) fn phase(&self) -> GatePhase {
        if self.failure.is_some() && self.cleanup != GatePhase::Releasing {
            GatePhase::Failed
        } else {
            self.cleanup
        }
    }
    pub(crate) fn attempt(&self) -> Option<AttemptId> {
        self.attempt
    }
    pub(crate) fn cleanup(&self) -> GatePhase {
        self.cleanup
    }
    pub(crate) fn has_surface(&self) -> bool {
        self.surface
    }
    pub(crate) fn blocked(&self) -> bool {
        self.blocked
    }
    pub(crate) fn failure(&self) -> Option<&ApplyFailure> {
        self.failure.as_ref()
    }
    fn matches(&self, attempt: AttemptId) -> bool {
        self.attempt == Some(attempt)
    }
    fn update(&self, changed: bool, create_native: bool, release_native: bool) -> GateUpdate {
        GateUpdate {
            changed,
            phase: self.phase(),
            attempt: self.attempt,
            create_native,
            release_native,
        }
    }
    pub(crate) fn unchanged(&self) -> GateUpdate {
        self.update(false, false, false)
    }

    /// Ids supplied by product reducer; native gate never allocates/replays one.
    pub(crate) fn begin_attempt(&mut self, attempt: AttemptId) -> GateUpdate {
        if self.attempt.is_some()
            || self.blocked
            || self
                .last_attempt
                .is_some_and(|last| last.get() >= attempt.get())
        {
            return self.unchanged();
        }
        self.last_attempt = Some(attempt);
        self.attempt = Some(attempt);
        self.cleanup = GatePhase::WaitingSurface;
        self.failure = None;
        self.surface = false;
        self.update(true, true, false)
    }
    /// Spawn failed before host effect was delivered: no destruction ack exists.
    pub(crate) fn retire_uncreated(&mut self, attempt: AttemptId) {
        if self.matches(attempt) && self.cleanup == GatePhase::WaitingSurface && !self.surface {
            self.attempt = None;
            self.cleanup = GatePhase::Idle;
        }
    }
    pub(crate) fn surface_ready(&mut self, attempt: AttemptId) -> GateUpdate {
        if !self.matches(attempt)
            || self.failure.is_some()
            || self.cleanup != GatePhase::WaitingSurface
            || self.surface
        {
            return self.unchanged();
        }
        self.surface = true;
        self.cleanup = GatePhase::Opening;
        self.update(true, false, false)
    }
    pub(crate) fn progress(
        &mut self,
        attempt: AttemptId,
        initialized: bool,
        started: bool,
    ) -> GateUpdate {
        if self.matches(attempt)
            && self.failure.is_none()
            && self.cleanup == GatePhase::Opening
            && self.surface
            && initialized
            && started
        {
            self.cleanup = GatePhase::Ready;
            self.update(true, false, false)
        } else {
            self.unchanged()
        }
    }
    pub(crate) fn fail(&mut self, attempt: AttemptId, failure: ApplyFailure) -> GateUpdate {
        if !self.matches(attempt) {
            return self.unchanged();
        }
        let secondary = |failure: &ApplyFailure| {
            matches!(
                failure.operation.as_str(),
                "surface_handoff" | "owner_unavailable"
            )
        };
        let replace = self
            .failure
            .as_ref()
            .is_none_or(|old| secondary(old) && !secondary(&failure));
        let changed = replace
            || self.surface
            || !matches!(self.cleanup, GatePhase::Stopping | GatePhase::Releasing);
        if replace {
            self.failure = Some(failure);
        }
        self.surface = false;
        if self.cleanup != GatePhase::Releasing {
            self.cleanup = GatePhase::Stopping;
        }
        self.update(changed, false, false)
    }
    pub(crate) fn surface_lost(&mut self, attempt: AttemptId, failure: ApplyFailure) -> GateUpdate {
        if !self.matches(attempt) || self.cleanup == GatePhase::Releasing {
            return self.unchanged();
        }
        self.blocked = true;
        self.fail(attempt, failure)
    }
    pub(crate) fn block(&mut self, attempt: AttemptId, failure: ApplyFailure) -> GateUpdate {
        if !self.matches(attempt) {
            return self.unchanged();
        }
        self.blocked = true;
        self.fail(attempt, failure)
    }
    pub(crate) fn stop(&mut self, attempt: AttemptId) -> GateUpdate {
        if !self.matches(attempt)
            || matches!(self.cleanup, GatePhase::Stopping | GatePhase::Releasing)
        {
            return self.unchanged();
        }
        self.cleanup = GatePhase::Stopping;
        self.surface = false;
        self.update(true, false, false)
    }
    pub(crate) fn owner_stopped(
        &mut self,
        attempt: AttemptId,
        failure: Option<ApplyFailure>,
    ) -> GateUpdate {
        if !self.matches(attempt) || self.cleanup == GatePhase::Releasing {
            return self.unchanged();
        }
        if let Some(failure) = failure {
            self.fail(attempt, failure);
        }
        self.surface = false;
        self.cleanup = GatePhase::Releasing;
        self.update(true, false, true)
    }
    pub(crate) fn native_released(&mut self, attempt: AttemptId) -> GateUpdate {
        if !self.matches(attempt) || self.cleanup != GatePhase::Releasing {
            return self.unchanged();
        }
        self.attempt = None;
        self.surface = false;
        self.cleanup = GatePhase::Idle;
        self.update(true, false, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        capture::{
            AudioSelection, CaptureMode, CapturedFourCc, DeviceIdentity, FrameRate, FrameSize,
            ModeRequest, UsbTopology,
        },
        failure::{Cause, FailureCategory, LifecycleFailure, Stage},
        state::DraftSettings,
    };
    use std::num::NonZeroU8;

    fn id(value: u64) -> AttemptId {
        AttemptId::new(value).unwrap()
    }
    fn failure(operation: &str) -> ApplyFailure {
        let identity = DeviceIdentity::new(
            0x32ed,
            0x3701,
            UsbTopology::new("controller".into(), vec![NonZeroU8::new(1).unwrap()]).unwrap(),
            None,
        )
        .unwrap();
        ApplyFailure::new(
            FailureCategory::Lifecycle(LifecycleFailure::Protocol),
            Stage::Unknown,
            Cause::Generic,
            DraftSettings {
                filters: crate::domain::filters::FilterChain::default(),
                video: ModeRequest {
                    identity,
                    mode: CaptureMode {
                        captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
                        size: FrameSize::new(1280, 720).unwrap(),
                        rate: FrameRate::new(60, 1).unwrap(),
                    },
                },
                audio: AudioSelection::default(),
            },
            operation,
            operation,
        )
    }
    fn opening() -> GateState {
        let mut state = GateState::new();
        assert!(state.begin_attempt(id(1)).create_native);
        state.surface_ready(id(1));
        state
    }
    #[test]
    fn handoff_occurs_once_and_stale_surface_cannot_publish() {
        let mut state = GateState::new();
        state.begin_attempt(id(1));
        assert!(!state.surface_ready(id(2)).changed);
        assert!(state.surface_ready(id(1)).changed);
        assert!(!state.surface_ready(id(1)).changed);
        assert_eq!(state.phase(), GatePhase::Opening);
    }
    #[test]
    fn ready_requires_initialized_started_and_matching_surface() {
        let mut state = GateState::new();
        state.begin_attempt(id(1));
        state.progress(id(1), true, true);
        assert_eq!(state.phase(), GatePhase::WaitingSurface);
        state.surface_ready(id(1));
        state.progress(id(2), true, true);
        state.progress(id(1), true, false);
        assert_eq!(state.phase(), GatePhase::Opening);
        state.progress(id(1), true, true);
        assert_eq!(state.phase(), GatePhase::Ready);
    }
    #[test]
    fn ack_then_native_release_required_and_release_never_reopens() {
        let mut state = opening();
        state.stop(id(1));
        assert!(!state.native_released(id(1)).changed);
        assert!(!state.begin_attempt(id(2)).create_native);
        let ack = state.owner_stopped(id(1), None);
        assert!(ack.release_native && !ack.create_native);
        assert!(!state.begin_attempt(id(2)).create_native);
        let release = state.native_released(id(1));
        assert!(release.changed && !release.create_native);
        assert!(state.begin_attempt(id(2)).create_native);
    }
    #[test]
    fn late_ready_during_stop_or_after_ack_never_reactivates() {
        let mut state = opening();
        state.stop(id(1));
        assert!(!state.progress(id(1), true, true).changed);
        state.owner_stopped(id(1), None);
        assert!(!state.progress(id(1), true, true).changed);
        assert_eq!(state.phase(), GatePhase::Releasing);
    }
    #[test]
    fn close_before_publication_rejects_delayed_token() {
        let mut state = GateState::new();
        state.begin_attempt(id(1));
        state.stop(id(1));
        assert!(!state.surface_ready(id(1)).changed);
        assert!(!state.has_surface());
    }
    #[test]
    fn duplicate_and_old_ack_release_have_no_native_effect() {
        let mut state = opening();
        assert!(!state.owner_stopped(id(2), None).release_native);
        state.owner_stopped(id(1), None);
        assert!(!state.owner_stopped(id(1), None).release_native);
        state.native_released(id(1));
        state.begin_attempt(id(2));
        assert!(!state.native_released(id(1)).changed);
        assert!(!state.owner_stopped(id(1), None).release_native);
        assert_eq!(state.attempt(), Some(id(2)));
    }
    #[test]
    fn forced_surface_loss_revokes_surface_and_poison_survives_ack_release() {
        for during_stop in [false, true] {
            let mut state = opening();
            state.progress(id(1), true, true);
            if during_stop {
                state.stop(id(1));
            }
            state.surface_lost(id(1), failure("surface_lost"));
            assert!(!state.has_surface());
            assert!(state.blocked());
            assert!(state.owner_stopped(id(1), None).release_native);
            state.native_released(id(1));
            assert!(!state.begin_attempt(id(2)).create_native);
        }
    }
    #[test]
    fn authorized_surface_loss_after_ack_is_not_failure() {
        let mut state = opening();
        state.stop(id(1));
        state.owner_stopped(id(1), None);
        assert!(!state.surface_lost(id(1), failure("surface_lost")).changed);
        assert!(!state.blocked());
    }
    #[test]
    fn unavailable_ack_blocks_without_inventing_release() {
        let mut state = opening();
        let update = state.block(id(1), failure("owner_disconnect"));
        assert!(!update.release_native);
        assert!(!state.native_released(id(1)).changed);
        assert!(!state.begin_attempt(id(2)).create_native);
    }
    #[test]
    fn genuine_failure_replaces_secondary_handoff_failure_only() {
        let mut state = opening();
        state.fail(id(1), failure("surface_handoff"));
        state.fail(id(1), failure("capture_session"));
        state.fail(id(1), failure("owner_unavailable"));
        assert_eq!(state.failure().unwrap().operation, "capture_session");
    }
    #[test]
    fn unexpected_ack_revokes_ready_and_only_then_releases() {
        let mut state = opening();
        state.progress(id(1), true, true);
        let ack = state.owner_stopped(id(1), Some(failure("playback")));
        assert_eq!(ack.phase, GatePhase::Releasing);
        assert!(ack.release_native && !state.has_surface());
    }
    #[test]
    fn attempt_ids_never_reused_after_real_release() {
        let mut state = opening();
        state.owner_stopped(id(1), None);
        state.native_released(id(1));
        assert!(!state.begin_attempt(id(1)).create_native);
        assert!(state.begin_attempt(id(u64::MAX)).create_native);
        state.owner_stopped(id(u64::MAX), None);
        state.native_released(id(u64::MAX));
        assert!(!state.begin_attempt(id(2)).create_native);
    }
}
