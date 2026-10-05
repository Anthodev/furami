//! Qt-thread composition: product coordinator plus capture/media adapters.

use crate::{
    app::{
        apply::ApplyCoordinator,
        control::{self, Command, ExpectedCleanup, ExpectedState},
        gate::GatePhase,
        ports::SubmitStatus,
    },
    capture::CaptureValidator,
    domain::{
        capture::{
            AudioAvailability, AudioSelection, AudioSilence, AudioSourceIdentity, CandidateId,
            LossEvidence, ObservationEpoch, PlaybackGain, RecoveryCandidate, SelectionToken,
            WatchId, WatchStamp,
        },
        state::{
            AttemptId, CleanupStatus, CommandRejection, DraftRevision, DraftSettings,
            InitialPlayback, PlaybackState, ProductModel, ProductPhase, StateIdentity,
        },
    },
    media::{controller::SurfaceToken, gate::GateRunner},
};

type Engine = ApplyCoordinator<CaptureValidator, GateRunner>;

pub(crate) struct UiUpdate {
    pub changed: bool,
    pub phase: GatePhase,
    pub generation: u64,
    pub restart_generation: u64,
    pub can_open: bool,
    pub can_restart: bool,
    /// Real product phase (reducer level), projected separately from the
    /// media gate phase so disconnected/recovering/selection states stay
    /// truthful while no media is open.
    pub product_phase: String,
    pub audio_status: String,
    pub audio_source: String,
    pub audio_desired: String,
    pub audio_diagnostic: String,
    pub failed: bool,
    pub diagnostic: String,
    pub recovery_evidence: String,
    pub recovery_stage: String,
    pub candidates: Vec<String>,
    pub paused: bool,
    /// This exact native attempt was opened paused, not paused after live playback.
    /// C++ latches concealment for its host until normal native retirement.
    pub prepared_paused: bool,
    pub volume_percent: i32,
    pub muted: bool,
    pub can_toggle_pause: bool,
    pub can_set_gain: bool,
    pub playback_status: String,
    pub create_native: bool,
    pub release_native: bool,
    pub quit: bool,
}

pub(crate) struct RuntimeCoordinator {
    engine: Option<Engine>,
    sources: Vec<AudioSourceIdentity>,
    dirty: bool,
    last_state: Option<(
        StateIdentity,
        DraftRevision,
        PlaybackGain,
        Option<PlaybackState>,
    )>,
    command_error: String,
    quit_empty: bool,
}
impl RuntimeCoordinator {
    pub(crate) fn new(
        prefix: String,
        settings: Option<DraftSettings>,
        gain: PlaybackGain,
        sources: Vec<AudioSourceIdentity>,
    ) -> Self {
        Self {
            engine: settings.map(|settings| {
                ApplyCoordinator::new(
                    settings,
                    gain,
                    CaptureValidator::new(),
                    GateRunner::new(prefix),
                )
            }),
            sources,
            dirty: true,
            last_state: None,
            command_error: String::new(),
            quit_empty: false,
        }
    }
    pub(crate) fn capture_selected(&self) -> bool {
        self.engine.is_some()
    }
    fn rejection(&mut self, error: impl std::fmt::Display) {
        self.command_error = error.to_string();
        self.dirty = true;
        tracing::warn!(error = %self.command_error, "qualification_command_rejected");
    }
    /// Open is a plain draft apply. It must never silently reconnect a lost
    /// session: reconnection is the separate explicit idempotent admission in
    /// [`Self::reconnect`], and `restart` stays the explicit forced restart.
    pub(crate) fn open(&mut self) -> UiUpdate {
        if let Some(engine) = &mut self.engine {
            let result = engine.apply(
                engine.model().state_identity(),
                engine.model().draft().revision,
            );
            if let Err(error) = result {
                self.rejection(error);
            } else {
                self.command_error.clear();
                self.dirty = true;
            }
        }
        self.poll()
    }
    pub(crate) fn restart(&mut self, expected_attempt: u64) -> UiUpdate {
        if let Some(engine) = &mut self.engine {
            let current = engine
                .model()
                .state_identity()
                .attempt()
                .map(AttemptId::get)
                .unwrap_or(0);
            if current != expected_attempt {
                self.rejection(CommandRejection::StaleState);
            } else {
                let result = engine.restart(engine.model().state_identity());
                if let Err(error) = result {
                    self.rejection(error);
                } else {
                    self.command_error.clear();
                    self.dirty = true;
                }
            }
        }
        self.poll()
    }
    pub(crate) fn close(&mut self, expected_attempt: u64) -> UiUpdate {
        if let Some(engine) = &mut self.engine {
            let current = engine
                .model()
                .state_identity()
                .attempt()
                .map(AttemptId::get)
                .unwrap_or(0);
            if current != expected_attempt {
                self.rejection(CommandRejection::StaleState);
            } else {
                let result = engine.close(engine.model().state_identity());
                if let Err(error) = result {
                    self.rejection(error);
                } else {
                    self.command_error.clear();
                    self.dirty = true;
                }
            }
        }
        self.poll()
    }
    /// Reconnect is an idempotent admission, never a forced restart: a healthy
    /// session no-ops, a pending recovery is joined, a disconnected last-valid
    /// session starts recovery, and shutdown refuses. There is no default
    /// capture open behind this path.
    pub(crate) fn reconnect(&mut self, expected_attempt: u64) -> UiUpdate {
        if let Some(engine) = &mut self.engine {
            let current = engine
                .model()
                .state_identity()
                .attempt()
                .map(AttemptId::get)
                .unwrap_or(0);
            if current != expected_attempt {
                self.rejection(CommandRejection::StaleState);
            } else {
                match engine.reconnect(engine.model().state_identity()) {
                    Ok(admission) => {
                        tracing::info!(?admission, "ui_reconnect_admitted");
                        self.command_error.clear();
                        self.dirty = true;
                    }
                    Err(error) => self.rejection(error),
                }
            }
        } else {
            self.rejection(CommandRejection::ReconnectUnavailable);
        }
        self.poll()
    }
    /// Explicit choice for an ambiguous recovery target. The token comes from
    /// a currently offered candidate; the expected attempt guards against a
    /// stale dialog acting on changed recovery state.
    pub(crate) fn choose_recovery(
        &mut self,
        expected_attempt: u64,
        watch: u64,
        epoch: u64,
        candidate: u64,
    ) -> UiUpdate {
        let token = (|| {
            Some(SelectionToken {
                stamp: WatchStamp {
                    watch: WatchId::new(watch)?,
                    epoch: ObservationEpoch::new(epoch)?,
                },
                candidate: CandidateId::new(candidate)?,
            })
        })();
        let Some(token) = token else {
            self.rejection("choose requires nonzero watch, epoch and candidate identifiers");
            return self.poll();
        };
        if let Some(engine) = &mut self.engine {
            let current = engine
                .model()
                .state_identity()
                .attempt()
                .map(AttemptId::get)
                .unwrap_or(0);
            if current != expected_attempt {
                self.rejection(CommandRejection::StaleState);
            } else {
                match engine.choose_recovery(engine.model().state_identity(), token) {
                    Ok(()) => {
                        self.command_error.clear();
                        self.dirty = true;
                    }
                    Err(error) => self.rejection(error),
                }
            }
        } else {
            self.rejection("no explicit startup capture selection");
        }
        self.poll()
    }
    pub(crate) fn quit(&mut self) -> UiUpdate {
        if let Some(engine) = &mut self.engine {
            engine.quit();
        } else {
            self.quit_empty = true;
        }
        self.dirty = true;
        self.poll()
    }
    pub(crate) fn surface_ready(&mut self, token: SurfaceToken) -> UiUpdate {
        if let Some(engine) = &mut self.engine {
            engine.runner_mut().surface_ready(token);
        }
        self.poll()
    }
    pub(crate) fn surface_lost(&mut self, attempt: AttemptId) -> UiUpdate {
        if let Some(engine) = &mut self.engine {
            engine.runner_mut().surface_lost(attempt);
        }
        self.poll()
    }
    pub(crate) fn wait_for_owner_ack(&mut self, attempt: AttemptId) -> String {
        match &mut self.engine {
            Some(engine) => engine
                .runner_mut()
                .wait_for_owner_ack(attempt)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default(),
            None => "surface_loss_barrier: no capture engine".into(),
        }
    }
    pub(crate) fn native_released(&mut self, attempt: AttemptId) -> UiUpdate {
        if let Some(engine) = &mut self.engine {
            engine.runner_mut().native_released(attempt);
        }
        self.poll()
    }
    pub(crate) fn pause(&mut self, attempt: AttemptId) -> SubmitStatus {
        let result = self
            .engine
            .as_mut()
            .map(|engine| engine.toggle_pause(attempt));
        self.dirty = true;
        match result {
            Some(Ok(status)) => {
                if status == SubmitStatus::Accepted {
                    self.command_error.clear();
                } else {
                    self.rejection(format!("pause submission {status:?}"));
                }
                status
            }
            Some(Err(error)) => {
                let status = match error {
                    CommandRejection::StaleState => SubmitStatus::StaleGeneration,
                    CommandRejection::ShuttingDown | CommandRejection::CleanupIncomplete => {
                        SubmitStatus::Closing
                    }
                    _ => SubmitStatus::NotReady,
                };
                self.rejection(error);
                status
            }
            None => SubmitStatus::NotReady,
        }
    }
    /// Immediate application gain from a visible control. Every admission
    /// outcome, including a rejected update or an unchanged accepted integer,
    /// forces presentation dirty so the next poll publishes an authoritative
    /// reconciliation instead of being suppressed by stamp equality.
    pub(crate) fn set_volume(&mut self, percent: i32) -> SubmitStatus {
        self.dirty = true;
        let Some(current) = self.engine.as_ref().map(|engine| engine.gain()) else {
            self.rejection("volume requires an active capture engine");
            return SubmitStatus::NotReady;
        };
        let Some(gain) = u8::try_from(percent)
            .ok()
            .filter(|value| *value <= 100)
            .and_then(|value| PlaybackGain::new(value, current.muted).ok())
        else {
            self.rejection("volume requires playback percent 0..100");
            return SubmitStatus::NotReady;
        };
        self.submit_gain(gain)
    }
    pub(crate) fn set_muted(&mut self, muted: bool) -> SubmitStatus {
        self.dirty = true;
        let gain = self.engine.as_ref().map(|engine| PlaybackGain {
            muted,
            ..engine.gain()
        });
        let Some(gain) = gain else {
            self.rejection("mute requires an active capture engine");
            return SubmitStatus::NotReady;
        };
        self.submit_gain(gain)
    }
    fn submit_gain(&mut self, gain: PlaybackGain) -> SubmitStatus {
        let status = match self.engine.as_mut() {
            Some(engine) => engine.set_gain(gain),
            None => SubmitStatus::NotReady,
        };
        if status == SubmitStatus::Accepted {
            self.command_error.clear();
        } else {
            self.rejection(format!("gain submission {status:?}"));
        }
        status
    }
    fn checked_state(
        engine: &Engine,
        expected: ExpectedState,
    ) -> Result<StateIdentity, CommandRejection> {
        let identity = engine.model().state_identity();
        let cleanup = match engine.model().cleanup() {
            CleanupStatus::Complete => ExpectedCleanup::Complete,
            CleanupStatus::Draining => ExpectedCleanup::Draining,
            CleanupStatus::Blocked { .. } => ExpectedCleanup::Blocked,
        };
        if engine.model().phase() != expected.phase
            || identity.operation().map(|id| id.get()).unwrap_or(0) != expected.apply
            || identity.attempt().map(AttemptId::get).unwrap_or(0) != expected.attempt
            || cleanup != expected.cleanup
        {
            Err(CommandRejection::StaleState)
        } else {
            Ok(identity)
        }
    }
    pub(crate) fn qualification_command(&mut self, line: &str) -> UiUpdate {
        let command = match control::parse(line) {
            Ok(command) => command,
            Err(error) => {
                self.rejection(error);
                return self.poll();
            }
        };
        if command == Command::Snapshot {
            self.dirty = true;
            return self.poll();
        }
        let Some(engine) = &mut self.engine else {
            self.rejection("no explicit startup capture selection");
            return self.poll();
        };
        let result: Result<(), String> = (|| {
            match command {
                Command::Video(revision, mode) => {
                    let mut settings = engine.model().draft().settings.clone();
                    settings.video.mode = mode;
                    engine
                        .edit_draft(revision, settings)
                        .map_err(|error| error.to_string())?;
                }
                Command::Identity(revision, identity) => {
                    let mut settings = engine.model().draft().settings.clone();
                    settings.video.identity = identity;
                    engine
                        .edit_draft(revision, settings)
                        .map_err(|error| error.to_string())?;
                }
                Command::Audio(revision, enabled) => {
                    let mut settings = engine.model().draft().settings.clone();
                    let source = settings.audio.source().cloned();
                    settings.audio = if enabled {
                        AudioSelection::Enabled {
                            source: source
                                .ok_or("enabling audio requires explicitly retained source")?,
                        }
                    } else {
                        AudioSelection::Disabled { retained: source }
                    };
                    engine
                        .edit_draft(revision, settings)
                        .map_err(|error| error.to_string())?;
                }
                Command::Source(revision, name) => {
                    let mut matches = self.sources.iter().filter(|source| source.name() == name);
                    let source = matches
                        .next()
                        .ok_or("source absent from enumerated startup audio catalog")?
                        .clone();
                    if matches.next().is_some() {
                        return Err("source ambiguous in enumerated startup audio catalog".into());
                    }
                    let mut settings = engine.model().draft().settings.clone();
                    settings.audio = if settings.audio.enabled() {
                        AudioSelection::Enabled { source }
                    } else {
                        AudioSelection::Disabled {
                            retained: Some(source),
                        }
                    };
                    engine
                        .edit_draft(revision, settings)
                        .map_err(|error| error.to_string())?;
                }
                Command::Apply(expected, revision) => {
                    let state =
                        Self::checked_state(engine, expected).map_err(|error| error.to_string())?;
                    engine
                        .apply(state, revision)
                        .map_err(|error| error.to_string())?;
                }
                Command::Restart(expected) => {
                    let state =
                        Self::checked_state(engine, expected).map_err(|error| error.to_string())?;
                    engine.restart(state).map_err(|error| error.to_string())?;
                }
                Command::Reconnect(expected) => {
                    let state =
                        Self::checked_state(engine, expected).map_err(|error| error.to_string())?;
                    let admission = engine.reconnect(state).map_err(|error| error.to_string())?;
                    tracing::info!(?admission, "qualification_reconnect_admitted");
                }
                Command::Choose(expected, token) => {
                    let state =
                        Self::checked_state(engine, expected).map_err(|error| error.to_string())?;
                    engine
                        .choose_recovery(state, token)
                        .map_err(|error| error.to_string())?;
                }
                Command::Close(expected) => {
                    let state =
                        Self::checked_state(engine, expected).map_err(|error| error.to_string())?;
                    engine.close(state).map_err(|error| error.to_string())?;
                }
                Command::Quit(expected) => {
                    Self::checked_state(engine, expected).map_err(|error| error.to_string())?;
                    engine.quit();
                }
                Command::Volume(attempt, percent) => {
                    if engine.model().state_identity().attempt() != Some(attempt) {
                        return Err("volume submission StaleGeneration".into());
                    }
                    let gain = PlaybackGain::new(percent, engine.gain().muted)
                        .map_err(|error| error.to_string())?;
                    let status = engine.set_gain(gain);
                    tracing::info!(attempt_id = attempt.get(), ?status, "qualification_volume");
                    if status != SubmitStatus::Accepted {
                        return Err(format!("volume submission {status:?}"));
                    }
                }
                Command::Mute(attempt, muted) => {
                    if engine.model().state_identity().attempt() != Some(attempt) {
                        return Err("mute submission StaleGeneration".into());
                    }
                    let gain = PlaybackGain {
                        muted,
                        ..engine.gain()
                    };
                    let status = engine.set_gain(gain);
                    tracing::info!(attempt_id = attempt.get(), ?status, "qualification_mute");
                    if status != SubmitStatus::Accepted {
                        return Err(format!("mute submission {status:?}"));
                    }
                }
                Command::Pause(attempt) => {
                    let status = engine.pause(attempt).map_err(|error| error.to_string())?;
                    if status != SubmitStatus::Accepted {
                        return Err(format!("pause submission {status:?}"));
                    }
                }
                Command::Resume(attempt) => {
                    let state = engine.model().state_identity();
                    engine
                        .resume(state, attempt)
                        .map_err(|error| error.to_string())?;
                }
                Command::Snapshot => {}
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.command_error.clear();
                self.dirty = true;
            }
            Err(error) => self.rejection(error),
        }
        self.poll()
    }
    pub(crate) fn poll(&mut self) -> UiUpdate {
        let Some(engine) = &mut self.engine else {
            let changed = std::mem::take(&mut self.dirty);
            return UiUpdate {
                changed,
                phase: if self.quit_empty {
                    GatePhase::QuitReady
                } else {
                    GatePhase::Idle
                },
                generation: 0,
                restart_generation: 0,
                can_open: false,
                can_restart: false,
                product_phase: if changed {
                    phase_name(ProductPhase::Stopped).into()
                } else {
                    String::new()
                },
                audio_status: if changed {
                    "Disabled".into()
                } else {
                    String::new()
                },
                audio_source: String::new(),
                audio_desired: String::new(),
                audio_diagnostic: String::new(),
                failed: false,
                diagnostic: if changed {
                    if self.command_error.is_empty() {
                        "No capture selected. Start with --capture-node, --capture-fourcc, --capture-size and --capture-rate.".into()
                    } else {
                        self.command_error.clone()
                    }
                } else {
                    String::new()
                },
                recovery_evidence: String::new(),
                recovery_stage: String::new(),
                candidates: Vec::new(),
                paused: false,
                prepared_paused: false,
                volume_percent: 0,
                muted: false,
                can_toggle_pause: false,
                can_set_gain: false,
                playback_status: "Unavailable".into(),
                create_native: false,
                release_native: false,
                quit: self.quit_empty,
            };
        };
        engine.poll();
        let native = engine.runner_mut().take_native_update();
        let identity = engine.model().state_identity();
        let active_playback = engine.model().active().map(|active| active.playback());
        let prepared_paused = prepared_paused_presentation(engine.model(), native.attempt);
        let stamp = (
            identity,
            engine.model().draft().revision,
            engine.gain(),
            active_playback,
        );
        let changed =
            std::mem::take(&mut self.dirty) || native.changed || self.last_state != Some(stamp);
        self.last_state = Some(stamp);
        // Desired audio is reported separately from actual availability so a
        // draft edit can never overwrite what the running (or lost) session
        // really had: the applied request wins, then recovery's frozen
        // applied settings, then the last valid settings, then the draft.
        let model = engine.model();
        let desired_settings = model
            .active()
            .map(|active| active.applied().settings())
            .or_else(|| model.recovery().map(|recovery| recovery.applied.settings()))
            .or_else(|| model.last_valid().map(|applied| applied.settings()))
            .unwrap_or(&model.draft().settings);
        let desired_source = desired_settings
            .audio
            .source()
            .map(|source| source.name().to_owned())
            .unwrap_or_default();
        let desired_audio_enabled = desired_settings.audio.enabled();
        let can_open = model.can_apply() || model.can_reconnect();
        let can_restart = model.can_restart() || model.can_reconnect();
        let restart_generation = identity.attempt().map(AttemptId::get).unwrap_or(0);
        let quit = model.shutdown_ready();
        let mut diagnostic = String::new();
        if changed {
            if let Some(rejection) = model.validation_rejection() {
                diagnostic.push_str(&rejection.failure.to_string());
            }
            if let Some(failures) = model.failures() {
                for failure in failures
                    .incumbent
                    .iter()
                    .chain(failures.candidate.iter())
                    .chain(failures.restore.iter())
                    .chain(failures.resume.iter())
                    .chain(failures.cleanup.iter())
                {
                    if !diagnostic.is_empty() {
                        diagnostic.push('\n');
                    }
                    diagnostic.push_str(&failure.to_string());
                }
            }
            if let Some(recovery) = model.recovery() {
                if !diagnostic.is_empty() {
                    diagnostic.push('\n');
                }
                diagnostic.push_str(&format!(
                    "Capture lost: {}",
                    loss_evidence_text(&recovery.evidence)
                ));
                diagnostic.push_str(&format!("\nRecovery stage: {}", recovery.failure));
                if !model.recovery_candidates().is_empty() {
                    diagnostic.push_str("\nRecovery needs an explicit source choice.");
                }
            }
            if !self.command_error.is_empty() {
                if !diagnostic.is_empty() {
                    diagnostic.push('\n');
                }
                diagnostic.push_str(&self.command_error);
            }
            log_snapshot(engine);
        }
        let phase = if quit {
            GatePhase::QuitReady
        } else if native.phase == GatePhase::Ready && engine.model().active().is_none() {
            GatePhase::Opening
        } else {
            native.phase
        };
        let paused = engine
            .model()
            .active()
            .is_some_and(|active| active.playback() == PlaybackState::Paused);
        let playback_status = match model.phase() {
            ProductPhase::ValidatingResume
            | ProductPhase::ClosingResume
            | ProductPhase::OpeningResume
            | ProductPhase::CleaningFailedResume => "Resuming",
            _ => match model.active() {
                Some(active) => match active.playback() {
                    PlaybackState::Live => "Live",
                    PlaybackState::PausePending { .. } => "Pausing",
                    PlaybackState::Paused => "Paused",
                },
                None => "Unavailable",
            },
        };
        // A recovered Paused session and a live silent video both keep pause
        // and gain admission; only shutdown and missing media block them.
        let can_toggle_pause = !quit
            && match model.phase() {
                ProductPhase::Active
                | ProductPhase::Paused
                | ProductPhase::ErrorWithActiveRestored => {
                    matches!(
                        active_playback,
                        Some(PlaybackState::Live) | Some(PlaybackState::Paused)
                    )
                }
                _ => false,
            };
        let can_set_gain = engine.gain_admission_open();
        let volume_percent = i32::from(engine.gain().volume_percent);
        let muted = engine.gain().muted;
        // Actual audio availability is canonical coordinator state, never the
        // desired draft: a draft edit or a desired request cannot masquerade
        // as a running route.
        let availability = engine.audio_availability().cloned();
        let (audio_status, audio_source, audio_diagnostic) = if changed {
            match &availability {
                None | Some(AudioAvailability::Disabled) => {
                    ("Disabled".to_owned(), String::new(), String::new())
                }
                Some(AudioAvailability::Silent { reason }) => {
                    let reason = match reason {
                        AudioSilence::WaitingForSource(error) => {
                            format!("waiting for source: {error}")
                        }
                        AudioSilence::PendingRoute => "waiting for audio route".to_owned(),
                        AudioSilence::Paused => "audio paused with live video".to_owned(),
                    };
                    ("Silent".to_owned(), String::new(), reason)
                }
                Some(AudioAvailability::Opening { epoch }) => (
                    "Opening".to_owned(),
                    String::new(),
                    format!("opening audio route (epoch {})", epoch.get()),
                ),
                Some(AudioAvailability::Active { source, .. }) => {
                    ("Active".to_owned(), source.name().to_owned(), String::new())
                }
                Some(AudioAvailability::Detaching { epoch }) => (
                    "Detaching".to_owned(),
                    String::new(),
                    format!("detaching audio route (epoch {})", epoch.get()),
                ),
                Some(AudioAvailability::Blocked { error, .. }) => {
                    ("Blocked".to_owned(), String::new(), error.to_string())
                }
            }
        } else {
            (String::new(), String::new(), String::new())
        };
        let (recovery_evidence, recovery_stage) = if changed {
            model
                .recovery()
                .map(|loss| {
                    (
                        loss_evidence_text(&loss.evidence).to_owned(),
                        loss.failure.to_string(),
                    )
                })
                .unwrap_or_default()
        } else {
            (String::new(), String::new())
        };
        let candidates: Vec<String> = if changed {
            model
                .recovery_candidates()
                .iter()
                .map(candidate_entry)
                .collect()
        } else {
            Vec::new()
        };
        let product_phase = if changed {
            phase_name(model.phase()).into()
        } else {
            String::new()
        };
        if changed
            && diagnostic.is_empty()
            && let Some(fatal) = engine.runner_mut().fatal_native_failure()
        {
            diagnostic = fatal.to_string();
        }
        if changed && diagnostic.is_empty() {
            diagnostic = if engine.runner_mut().report.is_empty() {
                format!(
                    "Requested: {:?}\nAudio requested: {desired_audio_enabled}\nOpen capture to apply draft.",
                    engine.model().draft().settings.video
                )
            } else {
                engine.runner_mut().report.clone()
            };
        }
        let runner = engine.runner_mut();
        UiUpdate {
            changed,
            phase,
            generation: native.attempt.map(AttemptId::get).unwrap_or(0),
            restart_generation,
            can_open,
            can_restart,
            product_phase,
            audio_status,
            audio_source,
            audio_desired: if changed {
                desired_source
            } else {
                String::new()
            },
            audio_diagnostic,
            failed: phase == GatePhase::Failed || runner.fatal_native_failure().is_some(),
            diagnostic,
            recovery_evidence,
            recovery_stage,
            candidates,
            paused,
            prepared_paused,
            volume_percent,
            muted,
            can_toggle_pause,
            can_set_gain,
            playback_status: playback_status.into(),
            create_native: native.create_native,
            release_native: native.release_native,
            quit,
        }
    }
    pub(crate) fn unchanged(&mut self) -> UiUpdate {
        self.poll()
    }
}

/// Initial playback is tied to the physical opening key, which can differ
/// from the validation request key. A verified paused commit retains that
/// origin; ordinary live-origin Pause must keep its frozen image visible.
fn prepared_paused_presentation(model: &ProductModel, attempt: Option<AttemptId>) -> bool {
    let Some(attempt) = attempt else {
        return false;
    };
    (model
        .opening()
        .is_some_and(|(key, _)| key.attempt == attempt)
        && model
            .opening_request()
            .is_some_and(|request| request.playback == InitialPlayback::Paused))
        || model.active().is_some_and(|active| {
            active.attempt() == attempt && active.initial_playback() == InitialPlayback::Paused
        })
}

/// Human-readable product phase: the reducer-level truth, projected next to
/// the media gate phase so disconnected/recovering/selection states stay
/// visible even while no media window exists.
fn phase_name(phase: ProductPhase) -> &'static str {
    match phase {
        ProductPhase::Stopped => "Stopped",
        ProductPhase::Active => "Active",
        ProductPhase::PausePending => "PausePending",
        ProductPhase::Paused => "Paused",
        ProductPhase::Validating => "Validating",
        ProductPhase::ClosingOld => "ClosingOld",
        ProductPhase::OpeningCandidate => "OpeningCandidate",
        ProductPhase::CleaningFailedCandidate => "CleaningFailedCandidate",
        ProductPhase::ValidatingPrior => "ValidatingPrior",
        ProductPhase::OpeningRestore => "OpeningRestore",
        ProductPhase::CleaningFailedRestore => "CleaningFailedRestore",
        ProductPhase::ValidatingResume => "ValidatingResume",
        ProductPhase::ClosingResume => "ClosingResume",
        ProductPhase::OpeningResume => "OpeningResume",
        ProductPhase::CleaningFailedResume => "CleaningFailedResume",
        ProductPhase::ErrorWithActiveRestored => "ErrorWithActiveRestored",
        ProductPhase::ErrorWithoutActive => "ErrorWithoutActive",
        ProductPhase::Disconnected => "Disconnected",
        ProductPhase::Recovering => "Recovering",
        ProductPhase::SelectionRequired => "SelectionRequired",
        ProductPhase::Stopping => "Stopping",
        ProductPhase::ShutdownReady => "ShutdownReady",
    }
}

/// Loss evidence distinguishes an ended stream (owner reported an end event)
/// from a removed capture source (the watcher observed the node disappear).
fn loss_evidence_text(evidence: &LossEvidence) -> String {
    match evidence {
        LossEvidence::StreamEnded { reason, error } => {
            format!("stream ended (reason {reason}, error {error})")
        }
        LossEvidence::Removed { stamp } => format!(
            "capture source removed (watch {}, observation {})",
            stamp.watch.get(),
            stamp.epoch.get()
        ),
    }
}

/// Machine-parseable candidate entry for the selection surface. Fields are
/// pipe-separated: "watch:epoch:candidate|description|vid:pid|controller|ports|serial".
/// The technical identity (USB VID/PID, controller, port chain, serial) is
/// always shown alongside the friendly description.
fn candidate_entry(candidate: &RecoveryCandidate) -> String {
    let identity = &candidate.identity;
    let topology = identity.topology();
    let ports = topology
        .ports()
        .iter()
        .map(|port| port.get().to_string())
        .collect::<Vec<_>>()
        .join(".");
    let serial = identity.serial().unwrap_or("-");
    format!(
        "{}:{}:{}|{}|{:04x}:{:04x}|{}|{}|{}",
        candidate.token.stamp.watch.get(),
        candidate.token.stamp.epoch.get(),
        candidate.token.candidate.get(),
        candidate.description,
        identity.vendor_id(),
        identity.product_id(),
        topology.controller(),
        ports,
        serial
    )
}

fn log_snapshot(engine: &Engine) {
    let model = engine.model();
    let identity = model.state_identity();
    let snapshot = serde_json::json!({
        "phase": model.phase(), "apply_id": identity.operation().map(|id| id.get()), "attempt_id": identity.attempt().map(AttemptId::get),
        "cleanup": match model.cleanup() { CleanupStatus::Complete => "Complete", CleanupStatus::Draining => "Draining", CleanupStatus::Blocked { .. } => "Blocked" },
        "draft_revision": model.draft().revision.get(), "draft": model.draft().settings,
        "last_valid": model.last_valid().map(|settings| settings.settings()),
        "active_attempt": model.active().map(|active| active.attempt().get()), "active_settings": model.active().map(|active| active.applied().settings()),
        "active_playback": model.active().map(|active| active.playback()),
        "failures": model.failures(), "validation_rejection": model.validation_rejection().map(|rejection| serde_json::json!({"request": rejection.request, "failure": rejection.failure})),
        "gain": engine.gain(), "can_apply": model.can_apply(), "can_restart": model.can_restart(), "can_reconnect": model.can_reconnect(), "shutdown_ready": model.shutdown_ready(),
    });
    tracing::info!(snapshot = %snapshot, "apply_runtime");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capture::{PreparedCapture, apply::fixture_prepared, linux::session_fixture},
        domain::{
            capture::{
                AudioEpoch, AudioError, AudioRouteReceipt, CaptureMode, CapturedFourCc, FrameRate,
                FrameSize,
            },
            failure::ApplyFailure,
            state::{ProductPhase, ValidationRequest},
        },
        media::controller::{
            BackendEvent, Generation, OwnerEndpoint, X11WindowId,
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
    fn validate(
        request: ValidationRequest,
    ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>) {
        let prepared = fixture_prepared(request.settings.clone());
        (request, prepared)
    }
    fn runtime() -> (RuntimeCoordinator, mpsc::Receiver<Driver>) {
        runtime_with_validator(validate)
    }
    fn runtime_with_validator(
        execute: fn(
            ValidationRequest,
        ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>),
    ) -> (RuntimeCoordinator, mpsc::Receiver<Driver>) {
        runtime_fixture(execute, None)
    }
    fn runtime_fixture(
        execute: fn(
            ValidationRequest,
        ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>),
        fail_spawn: Option<u64>,
    ) -> (RuntimeCoordinator, mpsc::Receiver<Driver>) {
        let (tx, rx) = mpsc::channel();
        let runner = GateRunner::with_spawner(move |generation, config| {
            if fail_spawn == Some(generation.get()) {
                return Err(crate::media::controller::MediaError::new(
                    "fixture_spawn",
                    "candidate owner spawn rejected",
                ));
            }
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
        let engine = ApplyCoordinator::new(
            settings(),
            PlaybackGain::default(),
            CaptureValidator::with_runner(execute),
            runner,
        );
        (
            RuntimeCoordinator {
                engine: Some(engine),
                sources: vec![
                    AudioSourceIdentity::new(
                        "explicit-source".into(),
                        vec![("device.serial".into(), "fixture".into())],
                    )
                    .unwrap(),
                ],
                dirty: true,
                last_state: None,
                command_error: String::new(),
                quit_empty: false,
            },
            rx,
        )
    }
    #[track_caller]
    fn await_update(
        runtime: &mut RuntimeCoordinator,
        predicate: impl Fn(&UiUpdate) -> bool,
    ) -> UiUpdate {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let update = runtime.poll();
            if predicate(&update) {
                return update;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "runtime event not delivered: state={:?}, canonical_audio={:?}, active_playback={:?}, failures={:?}, native_phase={:?}, playback={}, pause_admitted={}, gain_admitted={}, changed={}, audio_projection={}",
                runtime
                    .engine
                    .as_ref()
                    .map(|engine| engine.model().state_identity()),
                runtime
                    .engine
                    .as_ref()
                    .and_then(|engine| engine.audio_availability()),
                runtime
                    .engine
                    .as_ref()
                    .and_then(|engine| engine.model().active())
                    .map(|active| active.playback()),
                runtime
                    .engine
                    .as_ref()
                    .and_then(|engine| engine.model().failures()),
                update.phase,
                update.playback_status,
                update.can_toggle_pause,
                update.can_set_gain,
                update.changed,
                update.audio_status
            );
            std::thread::yield_now();
        }
    }
    fn start_live(runtime: &mut RuntimeCoordinator, drivers: &mpsc::Receiver<Driver>) -> Driver {
        let opening = runtime.open();
        if !opening.create_native {
            await_update(runtime, |update| update.create_native);
        }
        let driver = drivers.recv().unwrap();
        runtime.surface_ready(SurfaceToken {
            generation: Generation::new(1).unwrap(),
            xid: X11WindowId::new(71).unwrap(),
        });
        driver.initialized.recv().unwrap();
        let (load, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        await_update(runtime, |update| update.can_toggle_pause);
        driver
    }
    fn confirm_explicit_pause(runtime: &mut RuntimeCoordinator, driver: &Driver) {
        let attempt = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .active()
            .unwrap()
            .attempt();
        let pausing = runtime.qualification_command(&format!("pause {}", attempt.get()));
        assert_eq!(pausing.playback_status, "Pausing");
        assert!(!pausing.paused && !pausing.can_toggle_pause);
        snapshot_expected(runtime);
        let (outer, command) = driver.submitted.recv().unwrap();
        let crate::media::controller::BackendCommand::SetPaused {
            request,
            paused: true,
        } = command
        else {
            panic!("explicit pause transaction required");
        };
        driver.send(BackendEvent::PauseObserved(
            crate::media::controller::PauseObservation {
                request: Some(request),
                paused: true,
            },
        ));
        driver.send(BackendEvent::CommandReply {
            id: outer.get(),
            error: 0,
        });
        driver.fence();
        let paused = await_update(runtime, |update| update.playback_status == "Paused");
        assert!(paused.paused && paused.can_toggle_pause);
    }
    fn snapshot_expected(runtime: &RuntimeCoordinator) -> ExpectedState {
        let engine = runtime.engine.as_ref().unwrap();
        let model = engine.model();
        let identity = model.state_identity();
        let scalar = format!(
            "close {:?} {} {} Complete",
            model.phase(),
            identity.operation().map(|id| id.get()).unwrap_or(0),
            identity.attempt().map(AttemptId::get).unwrap_or(0),
        );
        let Command::Close(expected) = control::parse(&scalar).unwrap() else {
            panic!("snapshot close grammar");
        };
        assert_eq!(
            RuntimeCoordinator::checked_state(engine, expected),
            Ok(identity)
        );
        for wrong in [
            ExpectedState {
                phase: ProductPhase::Stopped,
                ..expected
            },
            ExpectedState {
                apply: expected.apply + 1,
                ..expected
            },
            ExpectedState {
                attempt: expected.attempt + 1,
                ..expected
            },
            ExpectedState {
                cleanup: ExpectedCleanup::Draining,
                ..expected
            },
        ] {
            assert_eq!(
                RuntimeCoordinator::checked_state(engine, wrong),
                Err(CommandRejection::StaleState)
            );
        }
        expected
    }
    fn cleanup(runtime: &mut RuntimeCoordinator) {
        let first = runtime.quit();
        let update = if first.release_native || first.quit {
            first
        } else {
            await_update(runtime, |update| update.release_native || update.quit)
        };
        if update.release_native {
            let released = runtime.native_released(AttemptId::new(update.generation).unwrap());
            if !released.quit {
                await_update(runtime, |update| update.quit);
            }
        }
    }
    #[test]
    fn draft_audio_and_video_commands_edit_only_until_explicit_apply() {
        let (mut runtime, drivers) = runtime();
        runtime.poll();
        runtime.qualification_command("draft-video 0 YUYV 1920x1080 30/1");
        runtime.qualification_command("draft-source 1 explicit-source");
        runtime.qualification_command("draft-audio 2 enable");
        let model = runtime.engine.as_ref().unwrap().model();
        assert_eq!(model.draft().revision.get(), 3);
        assert!(model.draft().settings.audio.enabled());
        assert_eq!(model.phase(), ProductPhase::Stopped);
        assert!(model.last_valid().is_none());
        assert!(drivers.try_recv().is_err());
        runtime.qualification_command("draft-audio 3 disable");
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .audio
                .source()
                .unwrap()
                .name(),
            "explicit-source"
        );
        cleanup(&mut runtime);
    }
    #[test]
    fn stale_full_state_or_revision_rejected_without_worker_or_owner_effect() {
        let (mut runtime, drivers) = runtime();
        for line in [
            "apply Active 0 0 Complete 0",
            "apply Stopped 1 0 Complete 0",
            "apply Stopped 0 1 Complete 0",
            "apply Stopped 0 0 Draining 0",
            "apply Stopped 0 0 Complete 1",
        ] {
            let update = runtime.qualification_command(line);
            assert!(update.changed && !update.create_native);
            assert!(!runtime.command_error.is_empty());
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().phase(),
                ProductPhase::Stopped
            );
            assert!(drivers.try_recv().is_err());
        }
        cleanup(&mut runtime);
    }
    #[test]
    fn paused_recovery_presentation_tracks_physical_open_and_not_ordinary_pause() {
        use crate::domain::{
            capture::{RecoveryObservation, SourcePresence, VideoPresence},
            state::ModelEffect,
        };

        let mut model = ProductModel::new(settings());
        model
            .apply(model.state_identity(), model.draft().revision)
            .unwrap();
        let initial_request = model.validation_request().unwrap().clone();
        let Some(ModelEffect::Open { key: old, .. }) = model.validation_succeeded(&initial_request)
        else {
            panic!("initial physical opening missing");
        };
        assert!(!prepared_paused_presentation(&model, Some(old.attempt)));
        model.open_verified(old);
        let pause = model.prepare_pause(old.attempt).unwrap();
        assert!(model.pause_admitted(old.attempt, pause));
        assert!(model.pause_observed(old.attempt, pause, true));
        assert_eq!(model.phase(), ProductPhase::Paused);
        assert!(!prepared_paused_presentation(&model, Some(old.attempt)));

        let watch = model.watch_target().unwrap().watch;
        let removal = ObservationEpoch::new(2).unwrap();
        let mut observation = RecoveryObservation {
            stamp: WatchStamp {
                watch,
                epoch: removal,
            },
            video: VideoPresence::Absent,
            audio: SourcePresence::Disabled,
            last_video_removal: Some(removal),
        };
        assert!(matches!(
            model.recovery_observed(&observation),
            Some(ModelEffect::Stop { attempt, .. }) if attempt == old.attempt
        ));
        assert!(model.barrier_complete(old.attempt).is_none());
        observation.stamp.epoch = ObservationEpoch::new(3).unwrap();
        observation.video = VideoPresence::Present;
        let Some(ModelEffect::Validate(request)) = model.recovery_observed(&observation) else {
            panic!("paused recovery validation missing");
        };
        assert_eq!(request.playback, InitialPlayback::Paused);
        let Some(ModelEffect::Open { key: recovered, .. }) = model.validation_succeeded(&request)
        else {
            panic!("paused recovery physical opening missing");
        };
        // Concealment starts with creation, before any OpenVerified receipt.
        assert!(prepared_paused_presentation(
            &model,
            Some(recovered.attempt)
        ));
        assert!(!prepared_paused_presentation(&model, Some(old.attempt)));
        assert!(!prepared_paused_presentation(&model, None));
        model.open_verified(recovered);
        assert_eq!(model.phase(), ProductPhase::Paused);
        assert!(prepared_paused_presentation(
            &model,
            Some(recovered.attempt)
        ));

        let (_, ModelEffect::Validate(resume)) = model
            .resume(model.state_identity(), recovered.attempt)
            .unwrap()
        else {
            panic!("fresh resume validation missing");
        };
        // Admitting Resume never makes the old prepared owner visible.
        assert!(prepared_paused_presentation(
            &model,
            Some(recovered.attempt)
        ));
        assert!(matches!(
            model.validation_succeeded(&resume),
            Some(ModelEffect::Stop { attempt, .. }) if attempt == recovered.attempt
        ));
        let Some(ModelEffect::Open { key: live, request }) =
            model.barrier_complete(recovered.attempt)
        else {
            panic!("fresh live resume opening missing");
        };
        assert_eq!(request.playback, InitialPlayback::Live);
        assert!(!prepared_paused_presentation(&model, Some(live.attempt)));
        model.open_verified(live);
        assert!(!prepared_paused_presentation(&model, Some(live.attempt)));
    }

    #[test]
    fn open_and_restart_routes_use_engine_native_barrier_and_last_valid_not_draft() {
        let (mut runtime, drivers) = runtime();
        runtime.poll();
        let initial = runtime.qualification_command("open Stopped 0 0 Complete 0");
        let open = if initial.create_native {
            initial
        } else {
            await_update(&mut runtime, |update| update.create_native)
        };
        assert_eq!(open.generation, 1);
        let first = drivers.recv().unwrap();
        runtime.surface_ready(SurfaceToken {
            generation: Generation::new(1).unwrap(),
            xid: X11WindowId::new(71).unwrap(),
        });
        first.initialized.recv().unwrap();
        let (load, _) = first.submitted.recv().unwrap();
        first.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        first.send(BackendEvent::FileLoaded);
        first.send(BackendEvent::PlaybackRestart);
        first.fence();
        await_update(&mut runtime, |update| update.phase == GatePhase::Ready);
        runtime.qualification_command("draft-video 0 YUYV 1920x1080 30/1");
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .last_valid()
                .unwrap()
                .settings()
                .video
                .mode,
            settings().video.mode
        );
        runtime.restart(1);
        let stopped = await_update(&mut runtime, |update| update.release_native);
        assert!(!stopped.create_native);
        assert!(drivers.try_recv().is_err());
        first.destroyed.recv().unwrap();
        let released = runtime.native_released(AttemptId::new(1).unwrap());
        let second_open = if released.create_native {
            released
        } else {
            await_update(&mut runtime, |update| update.create_native)
        };
        assert_eq!(second_open.generation, 2);
        let second = drivers.recv().unwrap();
        runtime.surface_ready(SurfaceToken {
            generation: Generation::new(2).unwrap(),
            xid: X11WindowId::new(71).unwrap(),
        });
        second.initialized.recv().unwrap();
        let (load, _) = second.submitted.recv().unwrap();
        second.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        second.send(BackendEvent::FileLoaded);
        second.send(BackendEvent::PlaybackRestart);
        second.fence();
        await_update(&mut runtime, |update| update.phase == GatePhase::Ready);
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .active()
                .unwrap()
                .applied()
                .settings()
                .video
                .mode,
            settings().video.mode
        );
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .video
                .mode
                .rate,
            FrameRate::new(30, 1).unwrap()
        );
        cleanup(&mut runtime);
        second.destroyed.recv().unwrap();
    }
    #[test]
    fn visible_pause_remains_false_until_correlated_observation_and_pending_action_is_rejected() {
        let (mut runtime, drivers) = runtime();
        let update = runtime.open();
        if !update.create_native {
            await_update(&mut runtime, |update| update.create_native);
        }
        let driver = drivers.recv().unwrap();
        let attempt = AttemptId::new(1).unwrap();
        runtime.surface_ready(SurfaceToken {
            generation: Generation::new(1).unwrap(),
            xid: X11WindowId::new(71).unwrap(),
        });
        driver.initialized.recv().unwrap();
        let (load, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        await_update(&mut runtime, |update| update.phase == GatePhase::Ready);
        assert_eq!(runtime.pause(attempt), SubmitStatus::Accepted);
        assert!(!runtime.poll().paused);
        assert_eq!(runtime.pause(attempt), SubmitStatus::NotReady);
        assert!(
            runtime
                .poll()
                .diagnostic
                .contains("playback transition already in progress")
        );
        let (outer, command) = driver.submitted.recv().unwrap();
        let crate::media::controller::BackendCommand::SetPaused {
            request,
            paused: true,
        } = command
        else {
            panic!("explicit pause")
        };
        driver.send(BackendEvent::PauseObserved(
            crate::media::controller::PauseObservation {
                request: None,
                paused: true,
            },
        ));
        driver.fence();
        assert!(!runtime.poll().paused);
        driver.send(BackendEvent::PauseObserved(
            crate::media::controller::PauseObservation {
                request: Some(request),
                paused: true,
            },
        ));
        driver.send(BackendEvent::CommandReply {
            id: outer.get(),
            error: 0,
        });
        driver.fence();
        assert!(runtime.poll().paused);
        assert_eq!(runtime.pause(attempt), SubmitStatus::Accepted);
        assert_eq!(
            runtime.engine.as_ref().unwrap().model().phase(),
            ProductPhase::ValidatingResume
        );
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn signed_volume_range_is_rejected_with_exact_diagnostic_without_mutation() {
        let (mut runtime, _) = runtime();
        runtime.poll();
        assert_eq!(runtime.set_volume(80), SubmitStatus::Accepted);
        assert_eq!(runtime.set_muted(true), SubmitStatus::Accepted);
        let preference = PlaybackGain::new(80, true).unwrap();
        let retained = runtime.poll();
        assert_eq!(retained.volume_percent, 80);
        assert!(retained.muted);
        assert_eq!(runtime.engine.as_ref().unwrap().gain(), preference);
        for percent in [-1, 101] {
            assert_eq!(runtime.set_volume(percent), SubmitStatus::NotReady);
            let update = runtime.poll();
            assert!(update.changed);
            assert_eq!(update.volume_percent, 80);
            assert!(update.muted);
            assert_eq!(runtime.engine.as_ref().unwrap().gain(), preference);
            assert!(
                update
                    .diagnostic
                    .contains("volume requires playback percent 0..100"),
                "{}",
                update.diagnostic
            );
        }
        cleanup(&mut runtime);
    }
    #[test]
    fn gain_setters_preserve_other_component_and_reconcile_unchanged_acceptance() {
        let (mut runtime, _) = runtime();
        runtime.poll();
        assert_eq!(runtime.set_volume(80), SubmitStatus::Accepted);
        assert_eq!(runtime.set_muted(true), SubmitStatus::Accepted);
        let muted = PlaybackGain::new(80, true).unwrap();
        assert_eq!(runtime.engine.as_ref().unwrap().gain(), muted);
        assert_eq!(runtime.set_volume(80), SubmitStatus::Accepted);
        let reconciled = runtime.poll();
        assert!(reconciled.changed);
        assert_eq!(reconciled.volume_percent, 80);
        assert!(reconciled.muted);
        assert!(reconciled.can_set_gain);
        assert_eq!(runtime.set_muted(false), SubmitStatus::Accepted);
        let unmuted = runtime.poll();
        assert!(unmuted.changed);
        assert_eq!(unmuted.volume_percent, 80);
        assert!(!unmuted.muted);
        assert_eq!(
            runtime.engine.as_ref().unwrap().gain(),
            PlaybackGain::new(80, false).unwrap()
        );
        cleanup(&mut runtime);
    }
    #[test]
    fn rejected_gain_admission_still_publishes_authoritative_preference() {
        let (mut runtime, drivers) = runtime();
        runtime.poll();
        assert_eq!(runtime.set_volume(64), SubmitStatus::Accepted);
        cleanup(&mut runtime);
        assert!(drivers.try_recv().is_err());
        let before = runtime.engine.as_ref().unwrap().gain();
        assert_eq!(runtime.set_volume(20), SubmitStatus::Closing);
        let update = runtime.poll();
        assert!(update.changed);
        assert_eq!(update.volume_percent, i32::from(before.volume_percent));
        assert_eq!(runtime.engine.as_ref().unwrap().gain(), before);
        assert!(update.diagnostic.contains("gain submission Closing"));
    }
    #[test]
    fn muted_setter_without_engine_is_rejected_and_published() {
        let mut runtime = {
            let (mut runtime, _) = runtime();
            runtime.engine = None;
            runtime
        };
        runtime.poll();
        assert_eq!(runtime.set_muted(true), SubmitStatus::NotReady);
        let update = runtime.poll();
        assert!(update.changed);
        assert!(!update.can_set_gain);
        assert_eq!(update.playback_status, "Unavailable");
        assert!(
            update
                .diagnostic
                .contains("mute requires an active capture engine")
        );
    }
    #[test]
    fn explicit_playback_commands_preserve_typed_guards_without_mutation() {
        let (mut runtime, drivers) = runtime();
        for command in ["pause 1", "resume 1"] {
            let before = runtime.engine.as_ref().unwrap().model().state_identity();
            runtime.qualification_command(command);
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().state_identity(),
                before
            );
            assert!(drivers.try_recv().is_err());
        }
        let driver = start_live(&mut runtime, &drivers);
        assert_eq!(
            runtime.engine.as_ref().unwrap().audio_availability(),
            Some(&AudioAvailability::Disabled)
        );
        for command in ["pause 2", "resume 2", "resume 1"] {
            let before = runtime.engine.as_ref().unwrap().model().state_identity();
            let rejected = runtime.qualification_command(command);
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().state_identity(),
                before
            );
            assert_eq!(rejected.playback_status, "Live");
            assert!(rejected.can_toggle_pause && !rejected.paused);
            assert!(driver.submitted.try_recv().is_err());
        }
        let pausing = runtime.qualification_command("pause 1");
        assert_eq!(pausing.playback_status, "Pausing");
        let pending = runtime.engine.as_ref().unwrap().model().state_identity();
        for command in ["pause 1", "resume 1"] {
            let rejected = runtime.qualification_command(command);
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().state_identity(),
                pending
            );
            assert!(!rejected.can_toggle_pause && !rejected.paused);
        }
        let (outer, command) = driver.submitted.recv().unwrap();
        let crate::media::controller::BackendCommand::SetPaused {
            request,
            paused: true,
        } = command
        else {
            panic!("explicit pause transaction required");
        };
        driver.send(BackendEvent::PauseObserved(
            crate::media::controller::PauseObservation {
                request: Some(request),
                paused: true,
            },
        ));
        driver.send(BackendEvent::CommandReply {
            id: outer.get(),
            error: 0,
        });
        driver.fence();
        let paused = await_update(&mut runtime, |update| update.playback_status == "Paused");
        assert!(paused.paused && paused.can_toggle_pause);
        for command in ["pause 2", "resume 2", "pause 1"] {
            let before = runtime.engine.as_ref().unwrap().model().state_identity();
            let rejected = runtime.qualification_command(command);
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().state_identity(),
                before
            );
            assert_eq!(rejected.playback_status, "Paused");
            assert!(rejected.can_toggle_pause && rejected.paused);
            assert!(driver.submitted.try_recv().is_err());
            assert!(drivers.try_recv().is_err());
        }
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
        for command in ["pause 1", "resume 1"] {
            let before = runtime.engine.as_ref().unwrap().model().state_identity();
            runtime.qualification_command(command);
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().state_identity(),
                before
            );
        }
    }

    #[test]
    fn explicit_pause_rejected_by_unavailable_port_keeps_live_state_and_reports_status() {
        let (mut runtime, drivers) = runtime();
        let driver = start_live(&mut runtime, &drivers);
        let before = runtime.engine.as_ref().unwrap().model().state_identity();
        // Replace only the admission port. Keep the real owner alive, then
        // restore its port before cleanup; this is not a physical owner failure.
        let unavailable =
            GateRunner::with_spawner(|_, _| panic!("no replacement opening requested"));
        let original =
            std::mem::replace(runtime.engine.as_mut().unwrap().runner_mut(), unavailable);
        let rejected = runtime.qualification_command("pause 1");
        *runtime.engine.as_mut().unwrap().runner_mut() = original;
        assert!(rejected.changed);
        assert_eq!(
            runtime.command_error,
            format!("pause submission {:?}", SubmitStatus::StaleGeneration)
        );
        assert!(rejected.diagnostic.contains(&runtime.command_error));
        assert_eq!(
            runtime.engine.as_ref().unwrap().model().state_identity(),
            before
        );
        assert_eq!(rejected.playback_status, "Live");
        assert!(rejected.can_toggle_pause && !rejected.paused);
        assert!(driver.submitted.try_recv().is_err());
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn validating_resume_status_outranks_paused_incumbent_and_cancelled_validation_blocks_gain() {
        use std::sync::{Mutex, OnceLock};
        static STARTED: OnceLock<mpsc::Sender<()>> = OnceLock::new();
        static RELEASE: OnceLock<Mutex<mpsc::Receiver<()>>> = OnceLock::new();
        fn execute(
            request: ValidationRequest,
        ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>) {
            if request.key.apply.get() == 2 {
                STARTED.get().unwrap().send(()).unwrap();
                let receiver = RELEASE
                    .get()
                    .unwrap()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let _ = receiver.recv();
            }
            validate(request)
        }
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        STARTED.set(started_tx).unwrap();
        RELEASE.set(Mutex::new(release_rx)).unwrap();
        let (mut runtime, drivers) = runtime_with_validator(execute);
        let driver = start_live(&mut runtime, &drivers);
        confirm_explicit_pause(&mut runtime, &driver);
        assert_eq!(runtime.set_volume(80), SubmitStatus::Accepted);
        assert_eq!(runtime.set_muted(true), SubmitStatus::Accepted);
        let resuming = runtime.qualification_command("resume 1");
        started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            runtime.engine.as_ref().unwrap().model().phase(),
            ProductPhase::ValidatingResume
        );
        assert_eq!(resuming.playback_status, "Resuming");
        assert!(resuming.paused && !resuming.can_toggle_pause);
        assert_eq!(resuming.volume_percent, 80);
        assert!(resuming.muted && resuming.can_set_gain);
        let before = runtime.engine.as_ref().unwrap().model().state_identity();
        for command in ["pause 1", "resume 1"] {
            let rejected = runtime.qualification_command(command);
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().state_identity(),
                before
            );
            assert_eq!(rejected.playback_status, "Resuming");
        }
        let closed = runtime.close(1);
        let retired = if closed.release_native {
            closed
        } else {
            await_update(&mut runtime, |update| update.release_native)
        };
        driver.destroyed.recv().unwrap();
        let waiting = runtime.native_released(AttemptId::new(1).unwrap());
        assert!(!waiting.can_set_gain);
        let preference = runtime.engine.as_ref().unwrap().gain();
        assert_eq!(runtime.set_volume(81), SubmitStatus::Closing);
        assert_eq!(runtime.set_muted(false), SubmitStatus::Closing);
        let rejected_gain = runtime.poll();
        assert!(!rejected_gain.can_set_gain);
        assert_eq!(runtime.engine.as_ref().unwrap().gain(), preference);
        assert_eq!(rejected_gain.volume_percent, 80);
        assert!(rejected_gain.muted);
        assert!(!retired.create_native && !waiting.create_native);
        let draining = runtime.engine.as_ref().unwrap().model().state_identity();
        for command in ["pause 1", "resume 1"] {
            let rejected = runtime.qualification_command(command);
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().state_identity(),
                draining
            );
            assert!(!rejected.can_toggle_pause && !rejected.can_set_gain);
            assert!(!rejected.create_native);
        }
        release_tx.send(()).unwrap();
        let drained = await_update(&mut runtime, |update| update.can_set_gain);
        assert!(!drained.create_native);
        assert!(drivers.try_recv().is_err());
        assert_eq!(runtime.set_volume(81), SubmitStatus::Accepted);
        cleanup(&mut runtime);
    }

    #[test]
    fn snapshot_lifecycle_commands_preserve_full_playback_identity_including_restored_active() {
        for restored in [false, true] {
            for paused in [false, true] {
                for verb in ["restart", "close", "quit"] {
                    let (mut runtime, drivers) = runtime_fixture(validate, restored.then_some(2));
                    let first = start_live(&mut runtime, &drivers);
                    let driver = if restored {
                        let expected = snapshot_expected(&runtime);
                        let restart = runtime.qualification_command(&format!(
                            "restart {:?} {} {} Complete",
                            expected.phase, expected.apply, expected.attempt,
                        ));
                        let retired = if restart.release_native {
                            restart
                        } else {
                            await_update(&mut runtime, |update| update.release_native)
                        };
                        first.destroyed.recv().unwrap();
                        let released =
                            runtime.native_released(AttemptId::new(retired.generation).unwrap());
                        let opening = if released.create_native {
                            released
                        } else {
                            await_update(&mut runtime, |update| update.create_native)
                        };
                        let restored_owner = drivers.recv().unwrap();
                        runtime.surface_ready(SurfaceToken {
                            generation: Generation::new(opening.generation).unwrap(),
                            xid: X11WindowId::new(71).unwrap(),
                        });
                        restored_owner.initialized.recv().unwrap();
                        let (load, _) = restored_owner.submitted.recv().unwrap();
                        restored_owner.send(BackendEvent::CommandReply {
                            id: load.get(),
                            error: 0,
                        });
                        restored_owner.send(BackendEvent::FileLoaded);
                        restored_owner.send(BackendEvent::PlaybackRestart);
                        restored_owner.fence();
                        await_update(&mut runtime, |update| update.can_toggle_pause);
                        assert_eq!(
                            runtime.engine.as_ref().unwrap().model().phase(),
                            ProductPhase::ErrorWithActiveRestored
                        );
                        restored_owner
                    } else {
                        first
                    };
                    if paused {
                        confirm_explicit_pause(&mut runtime, &driver);
                    }
                    let expected = snapshot_expected(&runtime);
                    let before = runtime.engine.as_ref().unwrap().model().state_identity();
                    let update = runtime.qualification_command(&format!(
                        "{verb} {:?} {} {} Complete",
                        expected.phase, expected.apply, expected.attempt,
                    ));
                    assert!(runtime.command_error.is_empty(), "{}", update.diagnostic);
                    assert_ne!(
                        runtime.engine.as_ref().unwrap().model().state_identity(),
                        before
                    );
                    if update.release_native {
                        // Quit before acknowledging a held release so restart
                        // cannot spawn another owner during test cleanup.
                        runtime.quit();
                        runtime.native_released(AttemptId::new(update.generation).unwrap());
                    }
                    cleanup(&mut runtime);
                    driver.destroyed.recv().unwrap();
                    assert!(drivers.try_recv().is_err());
                }
            }
        }
    }

    #[test]
    fn rejected_immediate_gain_never_mutates_stored_gain_or_draft() {
        let (mut runtime, _) = runtime();
        runtime.poll();
        runtime.qualification_command("volume 1 20");
        runtime.qualification_command("mute 1 on");
        assert_eq!(
            runtime.engine.as_ref().unwrap().gain(),
            PlaybackGain::default()
        );
        assert_eq!(
            runtime.engine.as_ref().unwrap().model().draft().settings,
            settings()
        );
        cleanup(&mut runtime);
    }
    #[test]
    fn qualification_gain_checks_attempt_before_preference_api_and_accepts_opening_without_surface()
    {
        let (mut runtime, drivers) = runtime();
        let update = runtime.open();
        if !update.create_native {
            await_update(&mut runtime, |update| update.create_native);
        }
        let driver = drivers.recv().unwrap();
        runtime.qualification_command("volume 1 80");
        runtime.qualification_command("mute 1 on");
        let admitted = PlaybackGain::new(80, true).unwrap();
        assert_eq!(runtime.engine.as_ref().unwrap().gain(), admitted);
        let rejected = runtime.qualification_command("volume 2 77");
        assert!(rejected.diagnostic.contains("StaleGeneration"));
        runtime.qualification_command("mute 2 off");
        assert_eq!(runtime.engine.as_ref().unwrap().gain(), admitted);
        assert_eq!(
            runtime.engine.as_ref().unwrap().model().draft().settings,
            settings()
        );
        assert!(driver.initialized.try_recv().is_err());
        runtime.surface_ready(SurfaceToken {
            generation: Generation::new(1).unwrap(),
            xid: X11WindowId::new(71).unwrap(),
        });
        driver.initialized.recv().unwrap();
        let (load, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        assert_eq!(
            driver.submitted.recv().unwrap().1,
            crate::media::controller::BackendCommand::SetGain(admitted)
        );
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn source_command_requires_enumerated_identity_and_stale_revision_preserves_draft() {
        let (mut runtime, _) = runtime();
        runtime.qualification_command("draft-source 0 missing");
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .revision
                .get(),
            0
        );
        runtime.qualification_command("draft-source 0 explicit-source");
        runtime.qualification_command("draft-audio 0 enable");
        let draft = runtime.engine.as_ref().unwrap().model().draft();
        assert_eq!(draft.revision.get(), 1);
        assert!(!draft.settings.audio.enabled());
        assert!(
            draft
                .settings
                .audio
                .source()
                .unwrap()
                .compatible_with(&runtime.sources[0])
        );
        cleanup(&mut runtime);
    }

    fn validate_enabled_audio(
        request: ValidationRequest,
    ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>) {
        let snapshot = session_fixture(&["/dev/video0"], request.settings.video.mode);
        let catalog = request
            .settings
            .audio
            .source()
            .map(|source| crate::capture::audio::AudioSource {
                identity: source.clone(),
                description: "fixture source".into(),
            })
            .into_iter()
            .collect::<Vec<_>>();
        let result = crate::capture::validate_prepared(
            request.settings.clone(),
            &snapshot,
            &catalog,
            request.watch,
        );
        (request, result)
    }

    #[test]
    fn live_audio_projection_ignores_unapplied_draft_and_clears_on_real_retirement() {
        let source_a = AudioSourceIdentity::new("source-a".into(), Vec::new()).unwrap();
        let source_b = AudioSourceIdentity::new("source-b".into(), Vec::new()).unwrap();
        let mut initial = settings();
        initial.audio = AudioSelection::Enabled {
            source: source_a.clone(),
        };
        let (tx, rx) = mpsc::channel();
        let runner = GateRunner::with_spawner(move |generation, config| {
            let requested = config.video.requested();
            let input = config
                .video
                .validate_snapshot(&session_fixture(&["/dev/video0"], requested.mode))
                .unwrap();
            let requested = input.requested().clone();
            let (driver, backend) = Driver::pair(Config::default());
            tx.send((driver, config.watch)).unwrap();
            OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || backend)
        });
        let engine = ApplyCoordinator::new(
            initial,
            PlaybackGain::default(),
            CaptureValidator::with_runner_and_audio_catalog(
                validate_enabled_audio,
                vec![crate::capture::audio::AudioSource {
                    identity: source_a.clone(),
                    description: "fixture source".into(),
                }],
            ),
            runner,
        );
        let mut runtime = RuntimeCoordinator {
            engine: Some(engine),
            sources: vec![source_a.clone(), source_b],
            dirty: true,
            last_state: None,
            command_error: String::new(),
            quit_empty: false,
        };
        let initial = runtime.qualification_command("open Stopped 0 0 Complete 0");
        let opening = if initial.create_native {
            initial
        } else {
            await_update(&mut runtime, |update| update.create_native)
        };
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .observation()
                .map(|observation| &observation.audio),
            Some(&crate::domain::capture::SourcePresence::Present)
        );
        let (driver, watch) = rx.recv().unwrap();
        runtime.surface_ready(SurfaceToken {
            generation: Generation::new(opening.generation).unwrap(),
            xid: X11WindowId::new(71).unwrap(),
        });
        driver.initialized.recv().unwrap();
        let (load, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        let epoch = AudioEpoch::new(1).unwrap();
        // The route is not readiness: the owner must first expose the admitted
        // audio epoch, and Opening must not grant playback controls.
        driver.send(BackendEvent::AudioAvailability(
            AudioAvailability::Opening { epoch },
        ));
        driver.fence();
        let audio_opening = await_update(&mut runtime, |update| update.audio_status == "Opening");
        assert_eq!(audio_opening.playback_status, "Unavailable");
        assert!(!audio_opening.can_toggle_pause);
        assert!(runtime.engine.as_ref().unwrap().model().active().is_none());
        let route = AudioRouteReceipt {
            epoch,
            stamp: watch,
            source_index: 0,
            source_output_index: 0,
            client_index: 0,
        };
        driver.send(BackendEvent::AudioAvailability(AudioAvailability::Active {
            source: source_a.clone(),
            route: route.clone(),
        }));
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        let active = await_update(&mut runtime, |update| update.can_toggle_pause);
        assert_eq!(
            runtime.engine.as_ref().unwrap().audio_availability(),
            Some(&AudioAvailability::Active {
                source: source_a.clone(),
                route: route.clone(),
            })
        );
        assert_eq!(active.playback_status, "Live");
        assert!(active.can_set_gain);
        assert_eq!(active.audio_source, "source-a");
        assert_eq!(active.audio_status, "Active");
        let edited = runtime.qualification_command("draft-source 0 source-b");
        assert_eq!(edited.audio_source, "source-a");
        assert_eq!(edited.audio_status, "Active");
        let disabled_draft = runtime.qualification_command("draft-audio 1 disable");
        assert_eq!(disabled_draft.audio_desired, "source-a");
        assert_eq!(disabled_draft.audio_source, "source-a");
        assert_eq!(disabled_draft.audio_status, "Active");
        confirm_explicit_pause(&mut runtime, &driver);
        let paused = runtime.engine.as_ref().unwrap().model().state_identity();
        driver.send(BackendEvent::AudioAvailability(
            AudioAvailability::Detaching { epoch },
        ));
        driver.fence();
        let detaching = await_update(&mut runtime, |update| update.audio_status == "Detaching");
        assert_eq!(detaching.playback_status, "Paused");
        assert!(detaching.can_toggle_pause && detaching.can_set_gain);
        driver.send(BackendEvent::AudioDetached {
            epoch,
            outcome: Ok(()),
        });
        driver.send(BackendEvent::AudioAvailability(AudioAvailability::Silent {
            reason: AudioSilence::WaitingForSource(AudioError::Cancelled),
        }));
        driver.fence();
        let silent = await_update(&mut runtime, |update| update.audio_status == "Silent");
        // Audio retirement must not withdraw video or pause controls while the
        // media owner itself stays healthy.
        assert!(silent.can_toggle_pause && silent.can_set_gain);
        assert_eq!(silent.playback_status, "Paused");
        assert!(silent.audio_source.is_empty());
        assert_eq!(silent.audio_desired, "source-a");
        assert_eq!(
            runtime.engine.as_ref().unwrap().audio_availability(),
            Some(&AudioAvailability::Silent {
                reason: AudioSilence::WaitingForSource(AudioError::Cancelled),
            })
        );
        for command in ["pause 1", "pause 2"] {
            let rejected = runtime.qualification_command(command);
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().state_identity(),
                paused
            );
            assert!(rejected.paused && rejected.can_toggle_pause && rejected.can_set_gain);
        }
        let close = runtime.close(opening.generation);
        let retired = if close.release_native {
            close
        } else {
            await_update(&mut runtime, |update| update.release_native)
        };
        assert_eq!(retired.audio_status, "Disabled");
        assert!(retired.audio_source.is_empty());
        assert_eq!(retired.audio_desired, "source-a");
        assert!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .audio_availability()
                .is_none()
        );
        driver.destroyed.recv().unwrap();
        let released = runtime.native_released(AttemptId::new(opening.generation).unwrap());
        assert_eq!(released.audio_status, "Disabled");
        assert!(runtime.engine.as_ref().unwrap().model().active().is_none());
        cleanup(&mut runtime);
    }

    #[test]
    fn reconnect_without_engine_or_history_is_rejected_actionably() {
        let mut absent = {
            let (mut runtime, _) = runtime();
            runtime.engine = None;
            runtime
        };
        absent.poll();
        let rejected = absent.reconnect(0);
        assert_eq!(rejected.product_phase, "Stopped");

        let (mut runtime, drivers) = runtime();
        runtime.poll();
        runtime.reconnect(0);
        assert!(drivers.try_recv().is_err());
        cleanup(&mut runtime);
    }
    #[test]
    fn stale_reconnect_attempt_is_rejected_without_effect() {
        let (mut runtime, drivers) = runtime();
        runtime.poll();
        let before = runtime.engine.as_ref().unwrap().model().state_identity();
        runtime.reconnect(7);
        assert_eq!(
            runtime.engine.as_ref().unwrap().model().state_identity(),
            before
        );
        assert!(drivers.try_recv().is_err());
        cleanup(&mut runtime);
    }
    #[test]
    fn choose_requires_nonzero_token_identifiers_and_stale_attempt_guard() {
        let (mut runtime, drivers) = runtime();
        runtime.poll();
        let before = runtime.engine.as_ref().unwrap().model().state_identity();
        runtime.choose_recovery(0, 0, 0, 0);
        assert_eq!(
            runtime.engine.as_ref().unwrap().model().state_identity(),
            before
        );
        runtime.choose_recovery(9, 1, 1, 1);
        assert_eq!(
            runtime.engine.as_ref().unwrap().model().state_identity(),
            before
        );
        assert!(drivers.try_recv().is_err());
        cleanup(&mut runtime);
    }
    #[test]
    fn poisoned_genuine_retirement_then_quit_keeps_fatal_surface_loss_status() {
        let (mut runtime, drivers) = runtime();
        let initial = runtime.qualification_command("open Stopped 0 0 Complete 0");
        let opening = if initial.create_native {
            initial
        } else {
            await_update(&mut runtime, |update| update.create_native)
        };
        let driver = drivers.recv().unwrap();
        runtime.surface_ready(SurfaceToken {
            generation: Generation::new(opening.generation).unwrap(),
            xid: X11WindowId::new(71).unwrap(),
        });
        driver.initialized.recv().unwrap();
        let (load, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        await_update(&mut runtime, |update| update.phase == GatePhase::Ready);
        let attempt = AttemptId::new(opening.generation).unwrap();
        let loss = runtime.surface_lost(attempt);
        assert!(loss.failed);
        let quitting = runtime.quit();
        let retired = if loss.release_native {
            loss
        } else if quitting.release_native {
            quitting
        } else {
            await_update(&mut runtime, |update| update.release_native)
        };
        assert!(retired.failed);
        driver.destroyed.recv().unwrap();
        let released = runtime.native_released(attempt);
        let final_update = if released.quit {
            released
        } else {
            await_update(&mut runtime, |update| update.quit)
        };
        assert_eq!(final_update.phase, GatePhase::QuitReady);
        assert!(final_update.failed);
        assert!(final_update.diagnostic.contains("surface_lost"));
        assert!(!final_update.can_open && !final_update.can_restart);
    }
}
