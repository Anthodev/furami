//! One owner thread, bounded intents, coalesced progress and a destruction-only ack.

use std::{
    num::{NonZeroU32, NonZeroU64},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, Thread},
    time::{Duration, Instant},
};

use super::session::{
    ObservedFacts, RequestedFacts, SessionError, SessionFacts, Verification, VerificationStatus,
};
use crate::app::ports::OpenReadiness;
#[cfg(test)]
use crate::capture::input::InputSpec;
use crate::capture::input::{CaptureSelection, SelectionError};
use crate::domain::capture::{
    AudioAvailability, AudioEpoch, AudioError, AudioSelection, AudioSourceIdentity, PlaybackGain,
    SelectedRouteAuthorization, WatchStamp,
};
use crate::domain::{
    failure::TreatmentFailure,
    output::OutputPlan,
    state::{
        InitialPlayback, PauseRequestId, TreatmentAttemptKey, TreatmentConfirmation, TreatmentPass,
    },
};

/// Renderer preparation is frozen before any native owner work is admitted.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedTreatments {
    pub(super) renderer: super::render_catalog::CompiledRendererSettings,
}
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub video: CaptureSelection,
    pub audio: AudioSelection,
    pub gain: PlaybackGain,
    pub output: OutputPlan,
    pub playback: InitialPlayback,
    pub watch: WatchStamp,
    pub selected_route: Option<SelectedRouteAuthorization>,
    pub treatments: PreparedTreatments,
    pub treatment_key: TreatmentAttemptKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct Generation(NonZeroU64);
impl Generation {
    pub fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X11WindowId(NonZeroU32);
impl X11WindowId {
    pub fn new(value: u64) -> Option<Self> {
        u32::try_from(value)
            .ok()
            .and_then(NonZeroU32::new)
            .map(Self)
    }
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceToken {
    pub generation: Generation,
    pub xid: X11WindowId,
}
#[derive(Clone, Debug, PartialEq)]
pub enum PlaybackIntent {
    ApplyTreatments {
        key: TreatmentAttemptKey,
        treatments: Box<PreparedTreatments>,
    },
    SetPaused {
        request: PauseRequestId,
        paused: bool,
    },
    SetGain(PlaybackGain),
    SetOutput(OutputPlan),
    DetachAudio {
        epoch: AudioEpoch,
    },
    AttachAudio {
        epoch: AudioEpoch,
        source: AudioSourceIdentity,
        stamp: WatchStamp,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitStatus {
    Accepted,
    StaleGeneration,
    NotReady,
    Closing,
    CapacityExceeded,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{code}: {diagnostic}")]
pub struct MediaError {
    pub code: &'static str,
    pub diagnostic: String,
    #[source]
    pub session: Option<Box<SessionError>>,
}
impl MediaError {
    pub(crate) fn new(code: &'static str, diagnostic: impl Into<String>) -> Self {
        Self {
            code,
            diagnostic: diagnostic.into(),
            session: None,
        }
    }
    pub(crate) fn from_session(error: SessionError) -> Self {
        Self {
            code: "capture_session",
            diagnostic: error.to_string(),
            session: Some(Box::new(error)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PauseObservation {
    pub request: Option<PauseRequestId>,
    pub paused: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ReadinessFacts {
    Capture(SessionFacts),
    QualificationVideo(ObservedFacts),
}
impl ReadinessFacts {
    pub(crate) fn capture(&self) -> Option<&SessionFacts> {
        match self {
            Self::Capture(facts) => Some(facts),
            Self::QualificationVideo(_) => None,
        }
    }
    #[cfg(test)]
    pub(crate) fn capture_mut(&mut self) -> Option<&mut SessionFacts> {
        match self {
            Self::Capture(facts) => Some(facts),
            Self::QualificationVideo(_) => None,
        }
    }
}

enum OwnerInput {
    Capture(RequestedFacts),
    QualificationVideo(crate::domain::capture::FrameSize),
    #[cfg(test)]
    Unspecified,
}
impl OwnerInput {
    fn capture(&self) -> Option<&RequestedFacts> {
        match self {
            Self::Capture(facts) => Some(facts),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub generation: Generation,
    pub initialized: bool,
    pub file_loaded: bool,
    pub playback_started: bool,
    pub session: Option<ReadinessFacts>,
    pub pause: Option<PauseObservation>,
    pub pause_pending: bool,
    pub stream_ended: Option<(i32, i32)>,
    pub failure: Option<MediaError>,
    pub audio: AudioAvailability,
    pub readiness: Option<OpenReadiness>,
    pub load_complete: bool,
    pub playback: InitialPlayback,
    pub audio_detached: Option<(AudioEpoch, Result<(), AudioError>)>,
    pub audio_epoch: Option<AudioEpoch>,
    pub treatments: Option<TreatmentSnapshot>,
    /// Independently retained revocation of the applied treatment. A newer
    /// unsuccessful operation must not hide this bounded health event.
    pub treatment_fault: Option<TreatmentSnapshot>,
    pub diagnostics: Option<RuntimeMediaObservation>,
}

/// Cold owner facts only; neither backend configuration requests nor readiness.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RuntimeMediaObservation {
    pub generation: u64,
    pub observation_epoch: u64,
    pub client_api: u64,
    pub libmpv_version: Arc<str>,
    pub backend: BackendObservation,
    pub helpers: super::loopback::HelperObservations,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BackendObservation {
    Unavailable(BackendUnavailable),
    Observed {
        video_output: Arc<str>,
        gpu_context: Arc<str>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackendUnavailable {
    NotObserved,
    Invalidated,
    NotConfigured,
    PropertyUnavailable,
    InvalidPayload,
    SubmissionRejected,
    EpochExhausted,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TreatmentSnapshot {
    pub key: TreatmentAttemptKey,
    pub sequence: u64,
    pub status: TreatmentStatus,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TreatmentStatus {
    Pending,
    Confirmed(TreatmentConfirmation),
    Failed(Box<TreatmentFailure>),
}
#[derive(Debug)]
pub(crate) struct OwnerStopped {
    pub generation: Generation,
    pub outcome: Result<(), MediaError>,
}

/// 0 = running, 1 = clean cancellation, 2 = fatal cancellation.
/// Fatal cancellation always dominates a concurrent clean close.
pub(crate) struct StopFlag {
    state: AtomicU8,
    reason: Mutex<Option<MediaError>>,
    audio_cancel: Arc<AtomicBool>,
    output: Mutex<Option<OutputPlan>>,
}
impl StopFlag {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
            reason: Mutex::new(None),
            audio_cancel: Arc::new(AtomicBool::new(false)),
            output: Mutex::new(None),
        }
    }
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new()
    }
    pub(crate) fn is_set(&self) -> bool {
        self.state.load(Ordering::Acquire) != 0
    }
    pub(crate) fn audio_cancel(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.audio_cancel)
    }
    fn request(&self, error: Option<MediaError>) {
        // Independent Pulse worker observes this level without waiting for mpv,
        // its command queue, or even an Opening command reply.
        let level = if error.is_some() { 2 } else { 1 };
        if let Some(error) = error
            && let Ok(mut reason) = self.reason.try_lock()
            && reason.is_none()
        {
            *reason = Some(error);
        }
        self.state.fetch_max(level, Ordering::Release);
        self.audio_cancel.store(true, Ordering::Release);
    }
    fn outcome(&self) -> Result<(), MediaError> {
        if self.state.load(Ordering::Acquire) == 2 {
            Err(self
                .reason
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .unwrap_or_else(|| MediaError::new("terminal_failure", "fatal owner cancellation")))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RequestId(NonZeroU64);
impl RequestId {
    pub(crate) fn get(self) -> u64 {
        self.0.get()
    }
    #[cfg(test)]
    pub(crate) fn for_test(value: u64) -> Self {
        assert!(value < (1 << 63));
        Self(NonZeroU64::new(value).expect("nonzero test request"))
    }
    fn next(last: &mut u64) -> Result<Self, MediaError> {
        *last = last
            .checked_add(1)
            .filter(|next| *next < (1 << 63))
            .ok_or_else(|| MediaError::new("request_exhausted", "mpv request ID exhausted"))?;
        Ok(Self(
            NonZeroU64::new(*last).expect("checked increment is nonzero"),
        ))
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BackendCommand {
    ApplyTreatments {
        key: TreatmentAttemptKey,
        treatments: Box<PreparedTreatments>,
    },
    LoadInput,
    SetPaused {
        request: PauseRequestId,
        paused: bool,
    },
    SetGain(PlaybackGain),
    SetOutput(OutputPlan),
    DetachAudio {
        epoch: AudioEpoch,
    },
    AttachAudio {
        epoch: AudioEpoch,
        source: AudioSourceIdentity,
        stamp: WatchStamp,
    },
}
#[derive(Clone, Copy)]
enum InFlight {
    LoadInput,
    SetPaused {
        request: PauseRequestId,
        paused: bool,
    },
    Control,
    Treatments {
        key: TreatmentAttemptKey,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BackendEvent {
    None,
    TreatmentResult {
        id: u64,
        key: TreatmentAttemptKey,
        result: Result<TreatmentConfirmation, Box<TreatmentFailure>>,
    },
    TreatmentFault {
        key: TreatmentAttemptKey,
        failure: Box<TreatmentFailure>,
    },
    FileLoaded,
    /// The production adapter's initial restart also requires fresh, exact
    /// decoded-frame VO configuration for this owner/load with force-window=no.
    /// It is not a presentation or continuous-video-health acknowledgment.
    PlaybackRestart,
    VideoReconfig,
    CommandReply {
        id: u64,
        error: i32,
    },
    EndFile {
        reason: i32,
        error: i32,
    },
    Shutdown,
    QueueOverflow,
    /// A copied log or an irrelevant event still counts toward the drain limit.
    Other,
    PauseObserved(PauseObservation),
    Observed {
        started: bool,
        facts: ObservedFacts,
    },
    AudioAvailability(AudioAvailability),
    Diagnostics(Box<RuntimeMediaObservation>),
    AudioDetached {
        epoch: AudioEpoch,
        outcome: Result<(), AudioError>,
    },
}
pub(crate) trait OwnerBackend {
    fn initialize(&mut self, token: SurfaceToken, stop: &StopFlag) -> Result<(), MediaError>;
    fn submit(&mut self, id: RequestId, command: BackendCommand) -> Result<(), MediaError>;
    fn next_event(&mut self) -> Result<BackendEvent, MediaError>;
    /// `None` means an asynchronous fresh read is pending. No owner-blocking
    /// property read is allowed once external audio can be opening or active.
    fn refresh_pause(&mut self) -> Result<Option<bool>, MediaError>;
    fn refresh_observed(&mut self, started: bool) -> Result<Option<ObservedFacts>, MediaError>;
    fn audio_status(&self) -> AudioAvailability {
        AudioAvailability::Disabled
    }
    fn initial_playback(&self) -> InitialPlayback {
        InitialPlayback::Live
    }
    fn audio_epoch(&self) -> Option<AudioEpoch> {
        None
    }
    fn initial_treatments(&mut self) -> Option<(TreatmentAttemptKey, PreparedTreatments)> {
        None
    }
    fn shutdown(&mut self) -> Result<(), MediaError>;
}

/// The endpoint contains Rust values only. Dropping it requests stop, never joins.
pub(crate) struct OwnerEndpoint {
    generation: Generation,
    surface: SyncSender<SurfaceToken>,
    commands: SyncSender<PlaybackIntent>,
    gain: Arc<Mutex<Option<PlaybackGain>>>,
    stop_flag: Arc<StopFlag>,
    published: AtomicBool,
    thread: Thread,
    latest: Arc<Mutex<Option<Snapshot>>>,
    stopped: Receiver<OwnerStopped>,
    buffered_ack: Option<OwnerStopped>,
    ack_received: bool,
    ack_disconnected: bool,
    worker: Option<thread::JoinHandle<()>>,
}
impl OwnerEndpoint {
    pub(crate) fn spawn(
        generation: Generation,
        prefix: String,
        config: SessionConfig,
    ) -> Result<Self, MediaError> {
        let requested = config.video.requested();
        let settings = crate::domain::state::DraftSettings {
            video: config.video.video_request().clone(),
            audio: config.audio.clone(),
            renderer: config.treatments.renderer.requested().clone(),
        };
        Self::spawn_task(
            generation,
            move |surface, commands, gain, stop, latest, snapshot| {
                if stop.is_set() {
                    return stop.outcome();
                }
                // Resolve physical identity again after incumbent cleanup. Prepared
                // paths are never rollback/reconnect targets and may have renumbered.
                let fresh = crate::capture::linux::discover().map_err(|error| {
                    MediaError::from_session(SessionError::prevalidation(
                        requested.clone(),
                        SelectionError::Capture(error),
                    ))
                })?;
                let input = crate::capture::revalidate_authorized_input(
                    &settings,
                    config.selected_route.as_ref(),
                    &config.video,
                    &fresh,
                )
                .map_err(|failure| {
                    MediaError::from_session(SessionError {
                        stage: failure.stage,
                        cause: failure.cause,
                        requested: requested.clone(),
                        diagnostic: failure.diagnostic,
                        evidence: failure.evidence,
                        source: None,
                    })
                })?;
                if stop.is_set() {
                    return stop.outcome();
                }
                let requested = input.requested().clone();
                log_capture_requested(generation, &requested);
                run_backend(
                    super::ffi::MpvBackend::new(
                        prefix,
                        input,
                        config.audio,
                        config.gain,
                        config.playback,
                        config.watch,
                        config.output,
                    )
                    .with_treatments(config.treatment_key, config.treatments),
                    surface,
                    commands,
                    gain,
                    stop,
                    latest,
                    snapshot,
                    0,
                    &OwnerInput::Capture(requested),
                )
            },
        )
    }

    pub(crate) fn spawn_qualification(
        generation: Generation,
        prefix: String,
        video: super::qualification::QualificationVideo,
        renderer: super::render_catalog::CompiledRendererSettings,
    ) -> Result<Self, MediaError> {
        let expected = video.size();
        Self::spawn_task(
            generation,
            move |surface, commands, gain, stop, latest, snapshot| {
                run_backend(
                    super::ffi::MpvBackend::for_qualification(prefix, video, renderer),
                    surface,
                    commands,
                    gain,
                    stop,
                    latest,
                    snapshot,
                    0,
                    &OwnerInput::QualificationVideo(expected),
                )
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn spawn_qualification_with_backend<B: OwnerBackend + Send + 'static>(
        generation: Generation,
        expected: crate::domain::capture::FrameSize,
        backend: B,
    ) -> Result<Self, MediaError> {
        Self::spawn_task(
            generation,
            move |surface, commands, gain, stop, latest, snapshot| {
                run_backend(
                    backend,
                    surface,
                    commands,
                    gain,
                    stop,
                    latest,
                    snapshot,
                    0,
                    &OwnerInput::QualificationVideo(expected),
                )
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn spawn_capture_with_backend<P, F, B>(
        generation: Generation,
        requested: RequestedFacts,
        prepare: P,
        factory: F,
    ) -> Result<Self, MediaError>
    where
        P: FnOnce() -> Result<InputSpec, SelectionError> + Send + 'static,
        F: FnOnce(InputSpec) -> B + Send + 'static,
        B: OwnerBackend + 'static,
    {
        Self::spawn_task(
            generation,
            move |surface, commands, gain, stop, latest, snapshot| {
                if stop.is_set() {
                    return stop.outcome();
                }
                let input = prepare().map_err(|error| {
                    MediaError::from_session(SessionError::prevalidation(requested.clone(), error))
                })?;
                if stop.is_set() {
                    return stop.outcome();
                }
                let requested = input.requested().clone();
                log_capture_requested(generation, &requested);
                run_backend(
                    factory(input),
                    surface,
                    commands,
                    gain,
                    stop,
                    latest,
                    snapshot,
                    0,
                    &OwnerInput::Capture(requested),
                )
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn spawn_with_backend<F, B>(
        generation: Generation,
        factory: F,
    ) -> Result<Self, MediaError>
    where
        F: FnOnce() -> B + Send + 'static,
        B: OwnerBackend + 'static,
    {
        Self::spawn_with_backend_seed(generation, factory, 0)
    }

    /// Lifecycle tests inject an already-prepared request. Unlike capture
    /// prevalidation tests, their fake backend lifetime exists even when stop
    /// wins before publication; shutdown must still precede the genuine ack.
    #[cfg(test)]
    pub(crate) fn spawn_with_backend_requested<F, B>(
        generation: Generation,
        requested: RequestedFacts,
        factory: F,
    ) -> Result<Self, MediaError>
    where
        F: FnOnce() -> B + Send + 'static,
        B: OwnerBackend + 'static,
    {
        Self::spawn_task(
            generation,
            move |surface, commands, gain, stop, latest, snapshot| {
                run_backend(
                    factory(),
                    surface,
                    commands,
                    gain,
                    stop,
                    latest,
                    snapshot,
                    0,
                    &OwnerInput::Capture(requested),
                )
            },
        )
    }

    #[cfg(test)]
    fn spawn_with_backend_seed<F, B>(
        generation: Generation,
        factory: F,
        request_seed: u64,
    ) -> Result<Self, MediaError>
    where
        F: FnOnce() -> B + Send + 'static,
        B: OwnerBackend + 'static,
    {
        Self::spawn_task(
            generation,
            move |surface, commands, gain, stop, latest, snapshot| {
                run_backend(
                    factory(),
                    surface,
                    commands,
                    gain,
                    stop,
                    latest,
                    snapshot,
                    request_seed,
                    &OwnerInput::Unspecified,
                )
            },
        )
    }

    fn spawn_task(
        generation: Generation,
        task: impl FnOnce(
            &Receiver<SurfaceToken>,
            &Receiver<PlaybackIntent>,
            &Mutex<Option<PlaybackGain>>,
            &StopFlag,
            &Mutex<Option<Snapshot>>,
            &mut Snapshot,
        ) -> Result<(), MediaError>
        + Send
        + 'static,
    ) -> Result<Self, MediaError> {
        let (surface, surface_rx) = mpsc::sync_channel(1);
        let (commands, command_rx) = mpsc::sync_channel(64);
        let (stopped_tx, stopped) = mpsc::sync_channel(1);
        let gain = Arc::new(Mutex::new(None));
        let owner_gain = Arc::clone(&gain);
        let latest = Arc::new(Mutex::new(None));
        let owner_latest = Arc::clone(&latest);
        let stop_flag = Arc::new(StopFlag::new());
        let owner_stop = Arc::clone(&stop_flag);
        let handle = thread::Builder::new()
            .name(format!("furami-mpv-{}", generation.get()))
            .spawn(move || {
                let mut snapshot = Snapshot {
                    generation,
                    initialized: false,
                    file_loaded: false,
                    playback_started: false,
                    session: None,
                    pause: None,
                    pause_pending: false,
                    stream_ended: None,
                    failure: None,
                    audio: AudioAvailability::Disabled,
                    readiness: None,
                    load_complete: false,
                    playback: InitialPlayback::Live,
                    audio_detached: None,
                    audio_epoch: None,
                    treatments: None,
                    treatment_fault: None,
                    diagnostics: None,
                };
                let outcome = task(
                    &surface_rx,
                    &command_rx,
                    &owner_gain,
                    &owner_stop,
                    &owner_latest,
                    &mut snapshot,
                );
                if let Err(error) = &outcome
                    && snapshot.failure.as_ref() != Some(error)
                {
                    owner_stop.request(Some(error.clone()));
                    snapshot.failure = Some(error.clone());
                    publish(&owner_latest, &snapshot);
                }
                let _ = stopped_tx.send(OwnerStopped {
                    generation,
                    outcome: owner_stop.outcome(),
                });
            })
            .map_err(|error| MediaError::new("owner_spawn", error.to_string()))?;
        let owner_thread = handle.thread().clone();
        let worker = Some(handle);
        Ok(Self {
            generation,
            surface,
            commands,
            gain,
            stop_flag,
            published: AtomicBool::new(false),
            thread: owner_thread,
            latest,
            stopped,
            buffered_ack: None,
            ack_received: false,
            ack_disconnected: false,
            worker,
        })
    }

    #[cfg(test)]
    pub(crate) fn rewrite_buffered_ack_generation(&mut self, generation: Generation) -> bool {
        if let Some(ack) = &mut self.buffered_ack {
            ack.generation = generation;
            true
        } else {
            false
        }
    }

    pub(crate) fn attach(&self, token: SurfaceToken) -> SubmitStatus {
        if token.generation != self.generation {
            return SubmitStatus::StaleGeneration;
        }
        if self.stop_flag.is_set() {
            return SubmitStatus::Closing;
        }
        if self.published.swap(true, Ordering::AcqRel) {
            return SubmitStatus::NotReady;
        }
        match self.surface.try_send(token) {
            Ok(()) => {
                self.thread.unpark();
                SubmitStatus::Accepted
            }
            Err(_) => {
                self.stop(
                    token.generation,
                    Some(MediaError::new(
                        "owner_disconnect",
                        "surface receiver disconnected",
                    )),
                );
                SubmitStatus::Closing
            }
        }
    }

    pub(crate) fn submit(&self, generation: Generation, intent: PlaybackIntent) -> SubmitStatus {
        if generation != self.generation {
            return SubmitStatus::StaleGeneration;
        }
        if self.stop_flag.is_set() {
            return SubmitStatus::Closing;
        }
        let command = match intent {
            PlaybackIntent::SetOutput(output) => {
                let mut slot = self
                    .stop_flag
                    .output
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if self.stop_flag.is_set() {
                    return SubmitStatus::Closing;
                }
                *slot = Some(output);
                self.thread.unpark();
                return SubmitStatus::Accepted;
            }
            PlaybackIntent::SetGain(gain) => {
                let mut slot = self.gain.lock().unwrap_or_else(|p| p.into_inner());
                if self.stop_flag.is_set() {
                    return SubmitStatus::Closing;
                }
                *slot = Some(gain);
                self.thread.unpark();
                return SubmitStatus::Accepted;
            }
            command => command,
        };
        match self.commands.try_send(command) {
            Ok(()) => {
                self.thread.unpark();
                SubmitStatus::Accepted
            }
            Err(TrySendError::Full(_)) => {
                self.stop(
                    generation,
                    Some(MediaError::new(
                        "command_overflow",
                        "64-command owner queue full",
                    )),
                );
                SubmitStatus::CapacityExceeded
            }
            Err(TrySendError::Disconnected(_)) => {
                self.stop(
                    generation,
                    Some(MediaError::new(
                        "owner_disconnect",
                        "command receiver disconnected",
                    )),
                );
                SubmitStatus::Closing
            }
        }
    }

    pub(crate) fn stop(&self, generation: Generation, error: Option<MediaError>) -> SubmitStatus {
        if generation != self.generation {
            return SubmitStatus::StaleGeneration;
        }
        self.stop_flag.request(error);
        self.thread.unpark();
        SubmitStatus::Accepted
    }

    pub(crate) fn take_stopped(&mut self) -> Result<Option<OwnerStopped>, MediaError> {
        // Expose destruction only after the session worker itself has retired.
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return Ok(None);
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| MediaError::new("owner_disconnect", "session worker panicked"))?;
        }
        if let Some(ack) = self.buffered_ack.take() {
            self.ack_received = true;
            return Ok(Some(ack));
        }
        if self.ack_received || self.ack_disconnected {
            return Ok(None);
        }
        match self.stopped.try_recv() {
            Ok(ack) => {
                self.ack_received = true;
                Ok(Some(ack))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.ack_disconnected = true;
                Err(MediaError::new(
                    "owner_disconnect",
                    "owner exited without destruction completion acknowledgment",
                ))
            }
        }
    }

    pub(crate) fn take_snapshot(&self) -> Option<Snapshot> {
        self.latest.try_lock().ok().and_then(|mut slot| slot.take())
    }

    /// Buffer the destruction acknowledgment without consuming it for the GUI reducer.
    pub(crate) fn wait_for_ack(&mut self) -> Result<(), MediaError> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| MediaError::new("owner_disconnect", "session worker panicked"))?;
        }
        if self.ack_received || self.buffered_ack.is_some() {
            return Ok(());
        }
        if !self.ack_disconnected {
            match self.stopped.recv() {
                Ok(ack) => {
                    self.buffered_ack = Some(ack);
                    return Ok(());
                }
                Err(_) => self.ack_disconnected = true,
            }
        }
        Err(MediaError::new(
            "owner_disconnect",
            "owner exited without destruction completion acknowledgment",
        ))
    }
}
impl Drop for OwnerEndpoint {
    fn drop(&mut self) {
        if !self.ack_received {
            self.stop(
                self.generation,
                Some(MediaError::new(
                    "endpoint_dropped",
                    "owner endpoint dropped before completion",
                )),
            );
        }
    }
}

struct BackendLifetime<B: OwnerBackend>(Option<B>);
impl<B: OwnerBackend> BackendLifetime<B> {
    fn shutdown(&mut self) -> Result<(), MediaError> {
        if let Some(mut backend) = self.0.take() {
            let outcome = backend.shutdown();
            drop(backend);
            outcome
        } else {
            Ok(())
        }
    }
}
impl<B: OwnerBackend> Drop for BackendLifetime<B> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn publish(latest: &Mutex<Option<Snapshot>>, snapshot: &Snapshot) {
    *latest.lock().unwrap_or_else(|p| p.into_inner()) = Some(snapshot.clone());
}

#[allow(clippy::too_many_arguments)]
fn run_backend<B: OwnerBackend>(
    backend: B,
    surface: &Receiver<SurfaceToken>,
    commands: &Receiver<PlaybackIntent>,
    gain: &Mutex<Option<PlaybackGain>>,
    stop: &StopFlag,
    latest: &Mutex<Option<Snapshot>>,
    snapshot: &mut Snapshot,
    request_seed: u64,
    input: &OwnerInput,
) -> Result<(), MediaError> {
    let mut backend = BackendLifetime(Some(backend));
    let mut outcome = match backend.0.as_mut() {
        Some(backend) => run_owner(
            backend,
            surface,
            commands,
            gain,
            stop,
            latest,
            snapshot,
            request_seed,
            input,
        ),
        None => Err(MediaError::new("owner_backend", "owner backend absent")),
    }
    .map_err(|error| match input.capture() {
        Some(requested) if error.session.is_none() && error.code != "cancelled" => {
            MediaError::from_session(SessionError::backend(
                requested.clone(),
                error.to_string(),
                None,
            ))
        }
        _ => error,
    });
    if let Err(error) = &outcome {
        stop.request(Some(error.clone()));
        snapshot.failure = Some(error.clone());
        publish(latest, snapshot);
    }
    tracing::info!(
        event_code = "owner_destroy_begin",
        generation = snapshot.generation.get(),
    );
    if let Err(error) = backend.shutdown() {
        stop.request(Some(error.clone()));
        if outcome.is_ok() {
            snapshot.failure = Some(error.clone());
            publish(latest, snapshot);
            outcome = Err(error);
        }
    }
    if snapshot.diagnostics.take().is_some() {
        publish(latest, snapshot);
    }
    tracing::info!(
        event_code = "owner_destroy_complete",
        generation = snapshot.generation.get(),
    );
    outcome
}

#[allow(clippy::too_many_arguments)]
fn run_owner<B: OwnerBackend>(
    backend: &mut B,
    surface: &Receiver<SurfaceToken>,
    commands: &Receiver<PlaybackIntent>,
    gain: &Mutex<Option<PlaybackGain>>,
    stop: &StopFlag,
    latest: &Mutex<Option<Snapshot>>,
    snapshot: &mut Snapshot,
    mut last_request: u64,
    input: &OwnerInput,
) -> Result<(), MediaError> {
    let mut reserved_restore = None;
    let mut applied_treatment = None;
    let mut applied_health_revoked = false;
    let token = loop {
        if stop.is_set() {
            return stop.outcome();
        }
        match surface.try_recv() {
            Ok(token) => break token,
            Err(TryRecvError::Empty) => thread::park_timeout(Duration::from_millis(10)),
            Err(TryRecvError::Disconnected) => {
                return Err(MediaError::new(
                    "surface_disconnect",
                    "surface handoff disconnected",
                ));
            }
        }
    };
    if stop.is_set() {
        return stop.outcome();
    }
    match backend.initialize(token, stop) {
        Err(error) if error.code != "cancelled" => return Err(error),
        Err(error) if !stop.is_set() => return Err(error),
        _ => {}
    }
    if stop.is_set() {
        return stop.outcome();
    }
    snapshot.initialized = true;
    snapshot.playback = backend.initial_playback();
    snapshot.audio = backend.audio_status();
    publish(latest, snapshot);
    let id = RequestId::next(&mut last_request)?;
    backend.submit(id, BackendCommand::LoadInput)?;
    let mut readiness_started = Some(Instant::now());
    let mut pending = Some((id, InFlight::LoadInput));
    loop {
        if stop.is_set() {
            return stop.outcome();
        }
        // Output loss must not wait behind a slow video load/pause readback.
        let output = stop.output.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(output) = output {
            if stop.is_set() {
                return stop.outcome();
            }
            backend.submit(
                RequestId::next(&mut last_request)?,
                BackendCommand::SetOutput(output),
            )?;
        }
        if pending.is_none() && snapshot.stream_ended.is_none() {
            let initial = if snapshot.readiness.is_some() {
                backend.initial_treatments().map(|(key, treatments)| {
                    PlaybackIntent::ApplyTreatments {
                        key,
                        treatments: Box::new(treatments),
                    }
                })
            } else {
                None
            };
            match initial.map_or_else(|| commands.try_recv(), Ok) {
                Ok(intent) => {
                    if stop.is_set() {
                        return stop.outcome();
                    }
                    let id = match &intent {
                        PlaybackIntent::ApplyTreatments { key, .. } => {
                            treatment_request(&mut last_request, &mut reserved_restore, *key)?
                        }
                        _ => RequestId::next(&mut last_request)?,
                    };
                    let command = match intent {
                        PlaybackIntent::ApplyTreatments { key, treatments } => {
                            BackendCommand::ApplyTreatments { key, treatments }
                        }
                        PlaybackIntent::SetPaused { request, paused } => {
                            BackendCommand::SetPaused { request, paused }
                        }
                        PlaybackIntent::DetachAudio { epoch } => {
                            BackendCommand::DetachAudio { epoch }
                        }
                        PlaybackIntent::AttachAudio {
                            epoch,
                            source,
                            stamp,
                        } => BackendCommand::AttachAudio {
                            epoch,
                            source,
                            stamp,
                        },
                        PlaybackIntent::SetGain(_) | PlaybackIntent::SetOutput(_) => {
                            unreachable!("latest-value controls")
                        }
                    };
                    if matches!(command, BackendCommand::SetPaused { .. }) {
                        snapshot.pause = None;
                        snapshot.pause_pending = matches!(input, OwnerInput::QualificationVideo(_));
                    }
                    let in_flight = match &command {
                        BackendCommand::ApplyTreatments { key, .. } => {
                            record_treatment(snapshot, *key, TreatmentStatus::Pending)?;
                            InFlight::Treatments { key: *key }
                        }
                        BackendCommand::SetPaused { request, paused } => InFlight::SetPaused {
                            request: *request,
                            paused: *paused,
                        },
                        _ => InFlight::Control,
                    };
                    backend.submit(id, command)?;
                    pending = Some((id, in_flight));
                    publish(latest, snapshot);
                }
                Err(TryRecvError::Empty) => {
                    let next = gain.lock().unwrap_or_else(|p| p.into_inner()).take();
                    if let Some(gain) = next {
                        if stop.is_set() {
                            return stop.outcome();
                        }
                        let id = RequestId::next(&mut last_request)?;
                        backend.submit(id, BackendCommand::SetGain(gain))?;
                        pending = Some((id, InFlight::Control));
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    return Err(MediaError::new(
                        "command_disconnect",
                        "intent endpoint disconnected",
                    ));
                }
            }
        }
        let mut drained = false;
        for _ in 0..64 {
            if stop.is_set() {
                return stop.outcome();
            }
            let event = backend.next_event()?;
            snapshot.audio_epoch = backend.audio_epoch();
            match event {
                BackendEvent::None => {
                    drained = true;
                    break;
                }
                BackendEvent::TreatmentResult { id, key, result } => {
                    if matches!(pending, Some((expected, InFlight::Treatments { key: expected_key })) if expected.get() == id && expected_key == key)
                    {
                        let status = match result {
                            Ok(confirmation)
                                if confirmation.key() == key && !applied_health_revoked =>
                            {
                                applied_treatment = Some(key);
                                snapshot.treatment_fault = None;
                                TreatmentStatus::Confirmed(confirmation)
                            }
                            Ok(_) => continue,
                            Err(failure) => TreatmentStatus::Failed(failure),
                        };
                        record_treatment(snapshot, key, status)?;
                        pending = None;
                        publish(latest, snapshot);
                    }
                }
                BackendEvent::TreatmentFault { key, failure } => {
                    if !applied_health_revoked
                        && applied_treatment == Some(key)
                        && failure.key == Some(key)
                    {
                        applied_health_revoked = true;
                        if snapshot
                            .treatments
                            .as_ref()
                            .is_some_and(|operation| operation.key == key)
                        {
                            record_treatment(snapshot, key, TreatmentStatus::Failed(failure))?;
                        } else {
                            record_treatment_fault(snapshot, key, failure)?;
                        }
                        publish(latest, snapshot);
                    }
                }
                BackendEvent::Other => {}
                BackendEvent::FileLoaded => {
                    if let Some(paused) = backend.refresh_pause()? {
                        record_pause(
                            snapshot,
                            PauseObservation {
                                request: None,
                                paused,
                            },
                            pending.as_ref(),
                        )?;
                    }
                    snapshot.file_loaded = true;
                    if snapshot.playback == InitialPlayback::Paused
                        && let Some(observed) = backend.refresh_observed(false)?
                    {
                        apply_observed(snapshot, input, false, observed)?;
                    }
                    publish(latest, snapshot);
                }
                BackendEvent::PlaybackRestart | BackendEvent::VideoReconfig => {
                    let started = matches!(event, BackendEvent::PlaybackRestart);
                    if let Some(observed) = backend.refresh_observed(started)? {
                        apply_observed(snapshot, input, started, observed)?;
                        if let Some(paused) = backend.refresh_pause()? {
                            record_pause(
                                snapshot,
                                PauseObservation {
                                    request: None,
                                    paused,
                                },
                                pending.as_ref(),
                            )?;
                        }
                        publish(latest, snapshot);
                    }
                }
                BackendEvent::Observed { started, facts } => {
                    apply_observed(snapshot, input, started, facts)?;
                    publish(latest, snapshot);
                }
                BackendEvent::PauseObserved(observation) => {
                    if record_pause(snapshot, observation, pending.as_ref())? {
                        publish(latest, snapshot);
                    }
                }
                BackendEvent::AudioAvailability(status) => {
                    snapshot.audio = status;
                    publish(latest, snapshot);
                }
                BackendEvent::Diagnostics(observation) => {
                    if observation.generation == snapshot.generation.get() {
                        snapshot.diagnostics = Some(*observation);
                        publish(latest, snapshot);
                    }
                }
                BackendEvent::AudioDetached { epoch, outcome } => {
                    snapshot.audio_detached = Some((epoch, outcome));
                    publish(latest, snapshot);
                }
                BackendEvent::CommandReply { id, error } => {
                    if let Some((expected, kind)) = pending.as_ref()
                        && expected.get() == id
                    {
                        if matches!(kind, InFlight::Treatments { .. }) {
                            continue; // Only the complete keyed treatment result ends this operation.
                        }
                        if error < 0 {
                            return Err(MediaError::new(
                                "command_reply",
                                format!("mpv async command failed: {error}"),
                            ));
                        }
                        if let InFlight::SetPaused { request, paused } = kind
                            && snapshot.pause
                                != Some(PauseObservation {
                                    request: Some(*request),
                                    paused: *paused,
                                })
                        {
                            return Err(MediaError::new(
                                "pause_property",
                                "pause command completed without matching expected readback",
                            ));
                        }
                        if matches!(kind, InFlight::SetPaused { .. }) && snapshot.pause_pending {
                            snapshot.pause_pending = false;
                            publish(latest, snapshot);
                        }
                        if matches!(kind, InFlight::LoadInput) {
                            snapshot.load_complete = true;
                            publish(latest, snapshot);
                        }
                        pending = None;
                        // Keep draining terminal health before publishing any
                        // readiness; the next intent is admitted next iteration.
                    }
                }
                BackendEvent::EndFile { reason, error } => {
                    snapshot.stream_ended = Some((reason, error));
                    tracing::info!(
                        event_code = "mpv_end_file",
                        generation = snapshot.generation.get(),
                        reason = end_file_reason(reason),
                        mpv_error = error,
                    );
                    if error < 0
                        || reason == 4
                        || matches!(input, OwnerInput::QualificationVideo(_))
                    {
                        return Err(MediaError::new(
                            "playback",
                            format!("mpv END_FILE reason={reason} error={error}"),
                        ));
                    }
                    publish(latest, snapshot);
                }
                BackendEvent::Shutdown => {
                    return Err(MediaError::new(
                        "mpv_shutdown",
                        "unexpected mpv SHUTDOWN; owner cleanup required",
                    ));
                }
                BackendEvent::QueueOverflow => {
                    return Err(MediaError::new(
                        "event_overflow",
                        "mpv event queue overflow; lifecycle evidence lost",
                    ));
                }
            }
        }
        let previous_readiness = snapshot.readiness;
        if readiness_started.is_some() {
            check_startup_readiness(
                snapshot,
                drained,
                stop,
                &mut readiness_started,
                input.capture(),
                Instant::now(),
            )?;
        } else if drained {
            update_readiness(snapshot);
        }
        if stop.is_set() {
            return stop.outcome();
        }
        if snapshot.readiness != previous_readiness {
            publish(latest, snapshot);
        }
        // Yield even when the bounded native batch had no NONE. A hostile event
        // stream must not turn deadline observation into an owner busy loop.
        thread::park_timeout(Duration::from_millis(10));
    }
}

const OPEN_READINESS_DEADLINE: Duration = Duration::from_secs(10);

/// This bounds initial readiness observation, not blocking native calls or
/// destruction. Once accepted, readiness never arms this owner's budget again.
fn check_startup_readiness(
    snapshot: &mut Snapshot,
    drained: bool,
    stop: &StopFlag,
    started: &mut Option<Instant>,
    requested: Option<&RequestedFacts>,
    now: Instant,
) -> Result<(), MediaError> {
    if stop.is_set() {
        return stop.outcome();
    }
    // A terminal already observed by this batch owns the outcome. Negative
    // native terminals return before this boundary; ordinary EOF remains ended.
    if snapshot.stream_ended.is_none()
        && started.is_some_and(|accepted| {
            now.saturating_duration_since(accepted) >= OPEN_READINESS_DEADLINE
        })
    {
        let diagnostic = "native playback readiness was not established within 10 seconds";
        let mut error = MediaError::new("startup_readiness_timeout", diagnostic);
        // Preserve the explicit operation through run_backend's generic wrapper.
        error.session = requested.map(|requested| {
            Box::new(SessionError::backend(
                requested.clone(),
                diagnostic.into(),
                None,
            ))
        });
        return Err(error);
    }
    if drained {
        update_readiness(snapshot);
    }
    if snapshot.readiness.is_some() {
        *started = None;
    }
    Ok(())
}

/// Reserve both cookies atomically. Output work advances the counter beyond the
/// reserved pair and can never consume the restoration cookie.
fn treatment_request(
    last: &mut u64,
    reserved: &mut Option<(TreatmentAttemptKey, RequestId)>,
    key: TreatmentAttemptKey,
) -> Result<RequestId, MediaError> {
    if key.pass == TreatmentPass::Restore
        && reserved.as_ref().is_some_and(|(candidate, _)| {
            candidate.apply == key.apply && candidate.attempt == key.attempt
        })
    {
        return Ok(reserved.take().expect("matched reservation").1);
    }
    let mut next = *last;
    let candidate = RequestId::next(&mut next)?;
    let restore = RequestId::next(&mut next)?;
    *last = next;
    *reserved = Some((key, restore));
    Ok(candidate)
}

fn superseded_failure(previous: &TreatmentFailure, incoming: &TreatmentFailure) -> bool {
    previous == incoming
        || (matches!(
            incoming.kind,
            crate::domain::failure::TreatmentErrorKind::Unconfirmed { .. }
        ) && !matches!(
            previous.kind,
            crate::domain::failure::TreatmentErrorKind::Unconfirmed { .. }
        ))
}

fn next_treatment_sequence(
    snapshot: &Snapshot,
    exhausted: &'static str,
) -> Result<u64, MediaError> {
    snapshot
        .treatments
        .as_ref()
        .map_or(0, |operation| operation.sequence)
        .max(
            snapshot
                .treatment_fault
                .as_ref()
                .map_or(0, |fault| fault.sequence),
        )
        .checked_add(1)
        .ok_or_else(|| MediaError::new("treatment_sequence", exhausted))
}

fn record_treatment(
    snapshot: &mut Snapshot,
    key: TreatmentAttemptKey,
    status: TreatmentStatus,
) -> Result<(), MediaError> {
    if let TreatmentStatus::Failed(incoming) = &status
        && let Some(TreatmentSnapshot {
            key: current,
            status: TreatmentStatus::Failed(previous),
            ..
        }) = &snapshot.treatments
        && *current == key
        && superseded_failure(previous, incoming)
    {
        return Ok(());
    }
    let sequence = next_treatment_sequence(snapshot, "treatment observation sequence exhausted")?;
    snapshot.treatments = Some(TreatmentSnapshot {
        key,
        sequence,
        status,
    });
    Ok(())
}

fn record_treatment_fault(
    snapshot: &mut Snapshot,
    key: TreatmentAttemptKey,
    failure: Box<TreatmentFailure>,
) -> Result<(), MediaError> {
    if let Some(TreatmentSnapshot {
        key: current,
        status: TreatmentStatus::Failed(previous),
        ..
    }) = &snapshot.treatment_fault
        && *current == key
        && superseded_failure(previous, &failure)
    {
        return Ok(());
    }
    let sequence = next_treatment_sequence(snapshot, "treatment fault sequence exhausted")?;
    snapshot.treatment_fault = Some(TreatmentSnapshot {
        key,
        sequence,
        status: TreatmentStatus::Failed(failure),
    });
    Ok(())
}

fn record_pause(
    snapshot: &mut Snapshot,
    observation: PauseObservation,
    pending: Option<&(RequestId, InFlight)>,
) -> Result<bool, MediaError> {
    match observation.request {
        Some(request) => {
            let Some((
                _,
                InFlight::SetPaused {
                    request: expected,
                    paused,
                },
            )) = pending
            else {
                return Ok(false);
            };
            if request != *expected {
                return Ok(false);
            }
            if observation.paused != *paused {
                return Err(MediaError::new(
                    "pause_property",
                    "fresh pause flag differs from requested state",
                ));
            }
            tracing::info!(
                event_code = "mpv_pause_observed",
                generation = snapshot.generation.get(),
                request_id = request.get(),
                paused = observation.paused,
            );
        }
        None => {
            if matches!(pending, Some((_, InFlight::SetPaused { .. })))
                || snapshot.pause.is_some_and(|pause| pause.request.is_some())
            {
                return Ok(false);
            }
        }
    }
    snapshot.pause = Some(observation);
    Ok(true)
}

fn apply_observed(
    snapshot: &mut Snapshot,
    input: &OwnerInput,
    started: bool,
    observed: ObservedFacts,
) -> Result<(), MediaError> {
    if started || snapshot.playback_started || snapshot.playback == InitialPlayback::Paused {
        match input {
            OwnerInput::Capture(requested) => {
                let facts = SessionFacts::verify(requested.clone(), observed)
                    .map_err(MediaError::from_session)?;
                log_capture_observed(snapshot.generation, &facts);
                snapshot.session = Some(ReadinessFacts::Capture(facts));
            }
            OwnerInput::QualificationVideo(expected) => {
                if let Some(size) = &observed.decoded_size {
                    if size.value != *expected {
                        return Err(MediaError::new(
                            "qualification_format",
                            "decoded fixture dimensions differ from validated header",
                        ));
                    }
                    snapshot.session = Some(ReadinessFacts::QualificationVideo(observed));
                }
            }
            #[cfg(test)]
            OwnerInput::Unspecified => {}
        }
        snapshot.playback_started |= started;
    }
    Ok(())
}

fn update_readiness(snapshot: &mut Snapshot) {
    snapshot.readiness = if !snapshot.initialized
        || !snapshot.file_loaded
        || !snapshot.load_complete
        || snapshot.session.as_ref().is_none_or(|facts| match facts {
            ReadinessFacts::Capture(_) => false,
            ReadinessFacts::QualificationVideo(observed) => observed.decoded_size.is_none(),
        })
        || snapshot.stream_ended.is_some()
        || snapshot.failure.is_some()
    {
        None
    } else {
        match (
            snapshot.playback,
            snapshot.pause.as_ref().map(|pause| pause.paused),
        ) {
            (InitialPlayback::Live, Some(false)) if snapshot.playback_started => {
                Some(OpenReadiness::Live)
            }
            (InitialPlayback::Paused, Some(true)) => Some(OpenReadiness::PausedPrepared),
            _ => None,
        }
    };
}

/// Bounded scalar projections for the private capture/owner diagnostics. No
/// path, URL or raw option text reaches the logging layer.
fn log_capture_requested(generation: Generation, requested: &RequestedFacts) {
    let identity = &requested.identity;
    let mode = &requested.mode;
    tracing::info!(
        event_code = "capture_requested",
        generation = generation.get(),
        vendor_id = identity.vendor_id() as u64,
        product_id = identity.product_id() as u64,
        width = mode.size.width() as u64,
        height = mode.size.height() as u64,
        fourcc = mode.captured_fourcc.kernel_value() as u64,
        rate_numerator = mode.rate.numerator() as u64,
        rate_denominator = mode.rate.denominator() as u64,
    );
}

fn log_capture_observed(generation: Generation, facts: &SessionFacts) {
    let mode = &facts.requested.mode;
    tracing::info!(
        event_code = "capture_observed",
        generation = generation.get(),
        width = mode.size.width() as u64,
        height = mode.size.height() as u64,
        fourcc = mode.captured_fourcc.kernel_value() as u64,
        rate_numerator = mode.rate.numerator() as u64,
        rate_denominator = mode.rate.denominator() as u64,
        status = verification_token(&facts.verification),
    );
}

fn verification_token(verification: &Verification) -> &'static str {
    use VerificationStatus::{Approximate, Configured, ObservedCompatible};
    let statuses = [
        verification.captured_fourcc,
        verification.decoded_size,
        verification.nominal_rate,
    ];
    if statuses.contains(&Configured) {
        "Configured"
    } else if statuses.contains(&Approximate) {
        "Approximate"
    } else if statuses.contains(&ObservedCompatible) {
        "ObservedCompatible"
    } else {
        "Unverified"
    }
}

fn end_file_reason(reason: i32) -> &'static str {
    match reason {
        0 => "Eof",
        2 => "Stop",
        3 => "Quit",
        4 => "Error",
        5 => "Redirect",
        _ => "Unknown",
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::mpsc::{self, Receiver, Sender};

    pub(crate) enum Input {
        Event(BackendEvent),
        Observe(ObservedFacts),
        Fence(Sender<()>),
    }

    pub(crate) struct Driver {
        pub events: Sender<Input>,
        pub submitted: Receiver<(RequestId, BackendCommand)>,
        pub initialized: Receiver<SurfaceToken>,
        pub destroyed: Receiver<bool>,
        pub initialize_release: Sender<()>,
        pub shutdown_release: Sender<()>,
        pub shutdown_started: Receiver<()>,
        pub quiesce_started: Receiver<()>,
        pub quiesce_release: Sender<()>,
    }

    #[derive(Default)]
    pub(crate) struct Config {
        pub initialization_error: Option<MediaError>,
        pub submission_error: Option<MediaError>,
        pub pause_error: bool,
        pub hold_initialize: bool,
        pub hold_shutdown: bool,
        pub creates_handle: bool,
        pub hold_quiesce: bool,
    }

    pub(crate) struct FakeBackend {
        config: Config,
        input: Receiver<Input>,
        submitted: Sender<(RequestId, BackendCommand)>,
        initialized: Sender<SurfaceToken>,
        destroyed: Sender<bool>,
        initialize_release: Receiver<()>,
        shutdown_release: Receiver<()>,
        shutdown_started: Sender<()>,
        quiesce_started: Sender<()>,
        quiesce_release: Receiver<()>,
        has_handle: bool,
        paused: bool,
        audio_epoch: Option<AudioEpoch>,
        observed: ObservedFacts,
        open_treatments: Option<(TreatmentAttemptKey, PreparedTreatments)>,
    }

    impl Driver {
        pub(crate) fn pair(config: Config) -> (Self, FakeBackend) {
            let (events, input) = mpsc::channel();
            let (submit_tx, submitted) = mpsc::channel();
            let (init_tx, initialized) = mpsc::channel();
            let (destroy_tx, destroyed) = mpsc::channel();
            let (initialize_release, init_rx) = mpsc::channel();
            let (shutdown_release, shutdown_rx) = mpsc::channel();
            let (shutdown_started_tx, shutdown_started) = mpsc::channel();
            let (quiesce_tx, quiesce_started) = mpsc::channel();
            let (quiesce_release, quiesce_rx) = mpsc::channel();
            (
                Self {
                    events,
                    submitted,
                    initialized,
                    destroyed,
                    initialize_release,
                    shutdown_release,
                    shutdown_started,
                    quiesce_started,
                    quiesce_release,
                },
                FakeBackend {
                    config,
                    input,
                    submitted: submit_tx,
                    initialized: init_tx,
                    destroyed: destroy_tx,
                    initialize_release: init_rx,
                    shutdown_release: shutdown_rx,
                    shutdown_started: shutdown_started_tx,
                    quiesce_started: quiesce_tx,
                    quiesce_release: quiesce_rx,
                    has_handle: false,
                    paused: false,
                    audio_epoch: None,
                    observed: ObservedFacts::default(),
                    open_treatments: None,
                },
            )
        }

        pub(crate) fn send(&self, event: BackendEvent) {
            self.events.send(Input::Event(event)).unwrap();
        }

        pub(crate) fn set_observed(&self, observed: ObservedFacts) {
            self.events.send(Input::Observe(observed)).unwrap();
        }

        pub(crate) fn fence(&self) {
            let (tx, rx) = mpsc::channel();
            self.events.send(Input::Fence(tx)).unwrap();
            rx.recv().unwrap();
        }
        /// Exercise the production batch-boundary timer with explicit monotonic
        /// time. Callers stage the resulting error through a real owner's stop
        /// and destruction path; this does not simulate ten wall-clock seconds.
        pub(crate) fn expired_initial_readiness(
            mut snapshot: Snapshot,
            requested: &RequestedFacts,
        ) -> MediaError {
            let accepted = Instant::now();
            check_startup_readiness(
                &mut snapshot,
                false,
                &StopFlag::new(),
                &mut Some(accepted),
                Some(requested),
                accepted + OPEN_READINESS_DEADLINE,
            )
            .unwrap_err()
        }

        /// Script a successful backend boundary only for the real submitted Open
        /// cookie/key. Production and observer tests never use this test driver.
        pub(crate) fn confirm_open_treatments(&self) -> TreatmentConfirmation {
            loop {
                let (id, command) = self
                    .submitted
                    .recv_timeout(Duration::from_secs(5))
                    .expect("owner did not submit configured initial treatments");
                match command {
                    BackendCommand::LoadInput => continue,
                    BackendCommand::ApplyTreatments { key, treatments }
                        if key.pass == TreatmentPass::Open =>
                    {
                        let confirmation = treatment_confirmation(
                            key,
                            InitialPlayback::Live,
                            treatments.renderer.requested().clone(),
                        );
                        self.send(BackendEvent::TreatmentResult {
                            id: id.get(),
                            key,
                            result: Ok(confirmation.clone()),
                        });
                        return confirmation;
                    }
                    other => panic!("expected initial treatment command, received {other:?}"),
                }
            }
        }
    }

    pub(crate) fn prepared_treatments(
        renderer: &crate::domain::render::RendererSettings,
    ) -> PreparedTreatments {
        PreparedTreatments {
            renderer: super::super::render_catalog::compile_renderer(
                renderer,
                &super::super::render_catalog::fixture_capabilities(),
            )
            .unwrap(),
        }
    }

    pub(crate) fn treatment_confirmation(
        key: TreatmentAttemptKey,
        playback: InitialPlayback,
        renderer: crate::domain::render::RendererSettings,
    ) -> TreatmentConfirmation {
        use crate::domain::state::RendererConfirmation;
        TreatmentConfirmation::checked(key, playback, RendererConfirmation::checked(key, renderer))
            .unwrap()
    }

    impl FakeBackend {
        pub(crate) fn with_open_treatments(
            mut self,
            key: TreatmentAttemptKey,
            treatments: PreparedTreatments,
        ) -> Self {
            self.open_treatments = Some((key, treatments));
            self
        }
    }

    impl OwnerBackend for FakeBackend {
        fn initialize(&mut self, token: SurfaceToken, _stop: &StopFlag) -> Result<(), MediaError> {
            self.has_handle = self.config.creates_handle;
            self.initialized.send(token).unwrap();
            if self.config.hold_initialize {
                self.initialize_release.recv().unwrap();
            }
            self.config.initialization_error.take().map_or(Ok(()), Err)
        }
        fn initial_treatments(&mut self) -> Option<(TreatmentAttemptKey, PreparedTreatments)> {
            self.open_treatments.take()
        }

        fn submit(&mut self, id: RequestId, command: BackendCommand) -> Result<(), MediaError> {
            if let Some(error) = self.config.submission_error.take() {
                return Err(error);
            }
            self.submitted.send((id, command)).unwrap();
            Ok(())
        }

        fn next_event(&mut self) -> Result<BackendEvent, MediaError> {
            match self.input.try_recv() {
                Ok(Input::Event(event)) => {
                    match &event {
                        BackendEvent::AudioAvailability(AudioAvailability::Opening { epoch }) => {
                            self.audio_epoch = Some(*epoch);
                        }
                        BackendEvent::AudioDetached {
                            epoch,
                            outcome: Ok(()),
                        } if self.audio_epoch == Some(*epoch) => {
                            self.audio_epoch = None;
                        }
                        _ => {}
                    }
                    Ok(event)
                }
                Ok(Input::Observe(observed)) => {
                    self.observed = observed;
                    Ok(BackendEvent::Other)
                }
                Ok(Input::Fence(tx)) => {
                    tx.send(()).unwrap();
                    Ok(BackendEvent::None)
                }
                Err(mpsc::TryRecvError::Empty) => Ok(BackendEvent::None),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(MediaError::new("fake_disconnect", "backend disconnected"))
                }
            }
        }

        fn audio_epoch(&self) -> Option<AudioEpoch> {
            self.audio_epoch
        }

        fn refresh_pause(&mut self) -> Result<Option<bool>, MediaError> {
            if self.config.pause_error {
                Err(MediaError::new("pause_property", "invalid pause property"))
            } else {
                Ok(Some(self.paused))
            }
        }

        fn refresh_observed(
            &mut self,
            _started: bool,
        ) -> Result<Option<ObservedFacts>, MediaError> {
            Ok(Some(self.observed.clone()))
        }
        fn shutdown(&mut self) -> Result<(), MediaError> {
            let _ = self.shutdown_started.send(());
            if self.config.hold_shutdown {
                self.shutdown_release.recv().unwrap();
            }
            if self.config.hold_quiesce {
                self.quiesce_started.send(()).unwrap();
                self.quiesce_release.recv().unwrap();
            }
            self.destroyed.send(self.has_handle).unwrap();
            self.has_handle = false;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Config, Driver};
    use super::*;

    fn pause_intent() -> PlaybackIntent {
        PlaybackIntent::SetPaused {
            request: PauseRequestId::new(1).unwrap(),
            paused: true,
        }
    }
    fn pause_command() -> BackendCommand {
        BackendCommand::SetPaused {
            request: PauseRequestId::new(1).unwrap(),
            paused: true,
        }
    }
    pub(super) fn generation() -> Generation {
        Generation::new(1).unwrap()
    }
    fn token() -> SurfaceToken {
        SurfaceToken {
            generation: generation(),
            xid: X11WindowId::new(47).unwrap(),
        }
    }
    pub(super) fn start(config: Config) -> (OwnerEndpoint, Driver) {
        let (driver, backend) = Driver::pair(config);
        let owner = OwnerEndpoint::spawn_with_backend(generation(), move || backend).unwrap();
        assert_eq!(owner.attach(token()), SubmitStatus::Accepted);
        driver.initialized.recv().unwrap();
        (owner, driver)
    }
    pub(super) fn load(driver: &Driver) -> RequestId {
        let (id, command) = driver.submitted.recv().unwrap();
        assert_eq!(command, BackendCommand::LoadInput);
        id
    }
    fn stop(owner: &mut OwnerEndpoint, driver: &Driver) -> OwnerStopped {
        owner.stop(generation(), None);
        owner.wait_for_ack().unwrap();
        assert!(driver.destroyed.recv().is_ok());
        owner.take_stopped().unwrap().unwrap()
    }

    #[test]
    fn diagnostics_follow_owner_snapshot_without_changing_readiness() {
        let (mut owner, driver) = start(Config::default());
        load(&driver);
        driver.fence();
        let before = owner.take_snapshot().unwrap();
        let observation = RuntimeMediaObservation {
            generation: generation().get(),
            observation_epoch: 7,
            client_api: 0x0002_0005,
            libmpv_version: "mpv v0.41.0".into(),
            backend: BackendObservation::Observed {
                video_output: "gpu-next".into(),
                gpu_context: "x11vk".into(),
            },
            helpers: super::super::loopback::HelperObservations::default(),
        };
        driver.send(BackendEvent::Diagnostics(Box::new(observation.clone())));
        driver.fence();
        let after = owner.take_snapshot().unwrap();
        assert_eq!(after.diagnostics, Some(observation.clone()));
        assert_eq!(after.readiness, before.readiness);
        assert_eq!(after.initialized, before.initialized);
        assert_eq!(after.file_loaded, before.file_loaded);
        assert_eq!(after.playback_started, before.playback_started);
        assert_eq!(after.failure, before.failure);
        let mut stale = observation.clone();
        stale.generation += 1;
        driver.send(BackendEvent::Diagnostics(Box::new(stale)));
        driver.fence();
        assert!(owner.take_snapshot().is_none());
        let mut helpers_changed = observation;
        helpers_changed.helpers.pw_loopback =
            super::super::loopback::HelperObservation::SpawnSucceeded;
        helpers_changed.helpers.pw_metadata =
            super::super::loopback::HelperObservation::SpawnFailed {
                io_kind: std::io::ErrorKind::PermissionDenied,
                os_code: Some(13),
            };
        driver.send(BackendEvent::Diagnostics(Box::new(helpers_changed.clone())));
        driver.fence();
        let after_helpers = owner.take_snapshot().unwrap();
        assert_eq!(after_helpers.diagnostics, Some(helpers_changed));
        assert_eq!(after_helpers.readiness, before.readiness);
        stop(&mut owner, &driver);
        assert!(owner.take_snapshot().unwrap().diagnostics.is_none());
    }

    fn treatment_key(apply: u64, pass: TreatmentPass) -> TreatmentAttemptKey {
        TreatmentAttemptKey {
            apply: crate::domain::state::ApplyId::new(apply).unwrap(),
            attempt: crate::domain::state::AttemptId::new(1).unwrap(),
            pass,
        }
    }
    fn treatments_intent(key: TreatmentAttemptKey) -> PlaybackIntent {
        PlaybackIntent::ApplyTreatments {
            key,
            treatments: Box::new(test_support::prepared_treatments(
                &crate::domain::render::RendererSettings::default(),
            )),
        }
    }
    fn treatment_failure(
        key: TreatmentAttemptKey,
        kind: crate::domain::failure::RendererErrorKind,
    ) -> Box<TreatmentFailure> {
        let renderer = crate::domain::failure::RendererFailure {
            property: crate::domain::render::RendererProperty::Scale,
            diagnostic: format!("{kind:?}"),
            kind,
        };
        Box::new(TreatmentFailure {
            key: Some(key),
            kind: crate::domain::failure::TreatmentErrorKind::Renderer(renderer),
            disposition: crate::domain::failure::TreatmentFailureDisposition::RestoreSameOwner,
        })
    }
    fn start_treatment() -> (OwnerEndpoint, Driver, RequestId, TreatmentAttemptKey) {
        let (owner, driver) = start(Config::default());
        let load = load(&driver);
        let key = treatment_key(1, TreatmentPass::Candidate);
        owner.submit(generation(), treatments_intent(key));
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        let (id, command) = driver
            .submitted
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(
            matches!(command, BackendCommand::ApplyTreatments { key: actual, .. } if actual == key)
        );
        (owner, driver, id, key)
    }

    #[test]
    fn treatment_pending_keeps_discrete_slot_until_exact_settled_result_not_command_reply() {
        let (mut owner, driver, id, key) = start_treatment();
        owner.submit(generation(), pause_intent());
        driver.send(BackendEvent::CommandReply {
            id: id.get(),
            error: -12,
        });
        driver.fence();
        assert!(driver.submitted.try_recv().is_err());
        assert!(matches!(
            owner.take_snapshot().unwrap().treatments.unwrap().status,
            TreatmentStatus::Pending
        ));
        let wrong_key = treatment_key(2, TreatmentPass::Candidate);
        for (cookie, actual) in [
            (id.get() + 1, key),
            (id.get(), wrong_key),
            (id.get(), treatment_key(1, TreatmentPass::Restore)),
        ] {
            driver.send(BackendEvent::TreatmentResult {
                id: cookie,
                key: actual,
                result: Ok(test_support::treatment_confirmation(
                    actual,
                    InitialPlayback::Live,
                    Default::default(),
                )),
            });
        }
        driver.fence();
        assert!(
            driver.submitted.try_recv().is_err(),
            "wrong cookies/keys/passes cannot retire pending work"
        );
        driver.send(BackendEvent::TreatmentResult {
            id: id.get(),
            key,
            result: Err(treatment_failure(
                key,
                crate::domain::failure::RendererErrorKind::Rejected { mpv_error: -12 },
            )),
        });
        let (_, command) = driver
            .submitted
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(command, pause_command());
        assert!(
            stop(&mut owner, &driver).outcome.is_ok(),
            "a clean treatment rejection is not a fatal owner error"
        );
    }

    #[test]
    fn treatment_sticky_confirmed_is_overwritten_by_same_key_fault_before_consumption() {
        let (mut owner, driver, id, key) = start_treatment();
        driver.send(BackendEvent::TreatmentResult {
            id: id.get(),
            key,
            result: Ok(test_support::treatment_confirmation(
                key,
                InitialPlayback::Live,
                Default::default(),
            )),
        });
        driver.send(BackendEvent::TreatmentFault {
            key,
            failure: treatment_failure(
                key,
                crate::domain::failure::RendererErrorKind::ReadbackMismatch,
            ),
        });
        driver.fence();
        let treatments = owner.take_snapshot().unwrap().treatments.unwrap();
        assert_eq!(treatments.key, key);
        assert_eq!(treatments.sequence, 3);
        assert!(matches!(treatments.status, TreatmentStatus::Failed(_)));
        driver.send(BackendEvent::TreatmentResult {
            id: id.get(),
            key,
            result: Ok(test_support::treatment_confirmation(
                key,
                InitialPlayback::Live,
                Default::default(),
            )),
        });
        driver.fence();
        assert!(
            owner.take_snapshot().is_none(),
            "duplicate positive result cannot replace the health fault"
        );
        assert!(stop(&mut owner, &driver).outcome.is_ok());
    }

    /// The applied-health lane must retain a late fault on the confirmed
    /// incumbent even while a newer candidate owns the latest-operation slot.
    #[test]
    fn applied_fault_lane_survives_newer_candidate_failure_in_same_drain_and_after_receipt() {
        for same_drain in [true, false] {
            let (mut owner, driver, id, confirmed) = start_treatment();
            driver.send(BackendEvent::TreatmentResult {
                id: id.get(),
                key: confirmed,
                result: Ok(test_support::treatment_confirmation(
                    confirmed,
                    InitialPlayback::Live,
                    Default::default(),
                )),
            });
            driver.fence();
            let applied = owner.take_snapshot().unwrap();
            assert_eq!(applied.treatments.as_ref().unwrap().key, confirmed);
            assert!(
                applied.treatment_fault.is_none(),
                "a positive result clears the applied-fault lane"
            );

            let candidate = treatment_key(2, TreatmentPass::Candidate);
            owner.submit(generation(), treatments_intent(candidate));
            let (candidate_id, command) = driver
                .submitted
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            assert!(
                matches!(command, BackendCommand::ApplyTreatments { key: actual, .. } if actual == candidate)
            );
            let mut rejected = treatment_failure(
                candidate,
                crate::domain::failure::RendererErrorKind::Rejected { mpv_error: -5 },
            );
            rejected.disposition = crate::domain::failure::TreatmentFailureDisposition::Untouched;
            driver.send(BackendEvent::TreatmentResult {
                id: candidate_id.get(),
                key: candidate,
                result: Err(rejected),
            });
            let fault = treatment_failure(
                confirmed,
                crate::domain::failure::RendererErrorKind::IncumbentMismatch,
            );
            if same_drain {
                driver.send(BackendEvent::TreatmentFault {
                    key: confirmed,
                    failure: fault,
                });
                driver.fence();
            } else {
                driver.fence();
                let interim = owner.take_snapshot().unwrap();
                assert_eq!(interim.treatments.as_ref().unwrap().key, candidate);
                assert!(matches!(
                    interim.treatments.as_ref().unwrap().status,
                    TreatmentStatus::Failed(_)
                ));
                assert!(
                    interim.treatment_fault.is_none(),
                    "a newer candidate failure is not an applied fault"
                );
                driver.send(BackendEvent::TreatmentFault {
                    key: confirmed,
                    failure: fault,
                });
                driver.fence();
            }
            let snapshot = owner.take_snapshot().unwrap();
            let latest = snapshot.treatments.as_ref().expect("latest operation");
            assert_eq!(
                latest.key, candidate,
                "newer candidate B keeps the latest-operation slot"
            );
            assert!(matches!(latest.status, TreatmentStatus::Failed(_)));
            let applied_fault = snapshot
                .treatment_fault
                .as_ref()
                .expect("applied fault lane retains the incumbent fault");
            assert_eq!(applied_fault.key, confirmed);
            let TreatmentStatus::Failed(failure) = &applied_fault.status else {
                panic!("applied fault lane must carry the typed failure");
            };
            assert_eq!(failure.key, Some(confirmed));
            assert!(
                applied_fault.sequence > latest.sequence,
                "fault sequence advances beyond the newer operation"
            );
            // A duplicate fault on the already-revoked applied key must not
            // advance the lane or replace the retained evidence.
            driver.send(BackendEvent::TreatmentFault {
                key: confirmed,
                failure: treatment_failure(
                    confirmed,
                    crate::domain::failure::RendererErrorKind::IncumbentMismatch,
                ),
            });
            driver.fence();
            if let Some(duplicate) = owner.take_snapshot() {
                assert_eq!(
                    duplicate
                        .treatment_fault
                        .as_ref()
                        .map(|fault| fault.sequence),
                    Some(applied_fault.sequence),
                    "duplicate fault must not advance the applied lane"
                );
            }
            assert!(stop(&mut owner, &driver).outcome.is_ok());
        }
    }

    #[test]
    fn treatment_terminal_and_native_overflow_share_snapshot_and_outrank_success() {
        for terminal in [
            BackendEvent::Shutdown,
            BackendEvent::QueueOverflow,
            BackendEvent::EndFile {
                reason: 4,
                error: -12,
            },
        ] {
            let (mut owner, driver, id, key) = start_treatment();
            driver.send(BackendEvent::TreatmentResult {
                id: id.get(),
                key,
                result: Ok(test_support::treatment_confirmation(
                    key,
                    InitialPlayback::Live,
                    Default::default(),
                )),
            });
            driver.send(terminal);
            assert!(owner.wait_for_ack().is_ok());
            let snapshot = owner.take_snapshot().unwrap();
            assert!(snapshot.failure.is_some());
            assert!(snapshot.readiness.is_none());
            assert!(owner.take_stopped().unwrap().unwrap().outcome.is_err());
            driver.destroyed.recv().unwrap();
        }
    }

    #[test]
    fn treatment_cookie_pair_reservation_is_checked_atomic_and_output_cannot_steal_restore() {
        let mut last = 0;
        let mut reserved = None;
        let key = treatment_key(1, TreatmentPass::Candidate);
        let candidate = treatment_request(&mut last, &mut reserved, key).unwrap();
        assert_eq!(candidate.get(), 1);
        assert_eq!(last, 2);
        assert_eq!(RequestId::next(&mut last).unwrap().get(), 3);
        assert_eq!(
            treatment_request(
                &mut last,
                &mut reserved,
                treatment_key(1, TreatmentPass::Restore)
            )
            .unwrap()
            .get(),
            2
        );
        assert!(reserved.is_none());
        assert_eq!(last, 3);
        let mut exhausted = (1 << 63) - 2;
        assert!(treatment_request(&mut exhausted, &mut reserved, key).is_err());
        assert_eq!(
            exhausted,
            (1 << 63) - 2,
            "a failed two-cookie reservation does not partially consume capacity"
        );
        assert!(reserved.is_none());
    }

    fn wait_for_owner_disconnect(owner: &mut OwnerEndpoint) -> MediaError {
        // Channel disconnection during unwind does not mean the worker has retired.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stopped = owner.take_stopped();
            assert!(!owner.ack_received);
            assert!(owner.buffered_ack.is_none());
            match stopped {
                Err(error) => return error,
                Ok(Some(_)) => panic!("owner panic fabricated a destruction acknowledgment"),
                Ok(None) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "owner disconnect was not reported within five seconds"
                    );
                    thread::yield_now();
                }
            }
        }
    }

    #[test]
    fn correlated_pause_waits_for_readback_and_old_generic_status_cannot_overwrite_it() {
        let (mut owner, driver) = start(Config::default());
        let load = load(&driver);
        let request = PauseRequestId::new(1).unwrap();
        owner.submit(
            generation(),
            PlaybackIntent::SetPaused {
                request,
                paused: true,
            },
        );
        owner.submit(
            generation(),
            PlaybackIntent::SetGain(PlaybackGain::default()),
        );
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        let (outer, command) = driver.submitted.recv().unwrap();
        assert_eq!(
            command,
            BackendCommand::SetPaused {
                request,
                paused: true
            }
        );
        driver.send(BackendEvent::PauseObserved(PauseObservation {
            request: Some(PauseRequestId::new(2).unwrap()),
            paused: true,
        }));
        driver.send(BackendEvent::PauseObserved(PauseObservation {
            request: None,
            paused: false,
        }));
        driver.fence();
        assert!(owner.take_snapshot().unwrap().pause.is_none());
        assert!(driver.submitted.try_recv().is_err());
        let observed = PauseObservation {
            request: Some(request),
            paused: true,
        };
        driver.send(BackendEvent::PauseObserved(observed));
        driver.send(BackendEvent::PauseObserved(PauseObservation {
            request: None,
            paused: false,
        }));
        driver.fence();
        assert_eq!(owner.take_snapshot().unwrap().pause, Some(observed));
        assert!(driver.submitted.try_recv().is_err());
        driver.send(BackendEvent::CommandReply {
            id: outer.get(),
            error: 0,
        });
        assert!(matches!(
            driver.submitted.recv().unwrap().1,
            BackendCommand::SetGain(_)
        ));
        driver.send(BackendEvent::PauseObserved(PauseObservation {
            request: None,
            paused: false,
        }));
        driver.fence();
        assert!(owner.take_snapshot().is_none());
        stop(&mut owner, &driver);
    }

    #[test]
    fn pause_set_failure_destroys_owner_without_publishing_paused() {
        let (mut owner, driver) = start(Config::default());
        let load = load(&driver);
        owner.submit(generation(), pause_intent());
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        let (outer, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::CommandReply {
            id: outer.get(),
            error: -5,
        });
        owner.wait_for_ack().unwrap();
        assert_eq!(
            owner
                .take_stopped()
                .unwrap()
                .unwrap()
                .outcome
                .unwrap_err()
                .code,
            "command_reply"
        );
        assert!(
            !owner
                .take_snapshot()
                .unwrap()
                .pause
                .is_some_and(|pause| pause.paused)
        );
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn pause_success_without_expected_correlated_readback_is_terminal() {
        for observed in [None, Some(false)] {
            let (mut owner, driver) = start(Config::default());
            let load = load(&driver);
            let request = PauseRequestId::new(1).unwrap();
            owner.submit(
                generation(),
                PlaybackIntent::SetPaused {
                    request,
                    paused: true,
                },
            );
            driver.send(BackendEvent::CommandReply {
                id: load.get(),
                error: 0,
            });
            let (outer, _) = driver.submitted.recv().unwrap();
            if let Some(paused) = observed {
                driver.send(BackendEvent::PauseObserved(PauseObservation {
                    request: Some(request),
                    paused,
                }));
            }
            driver.send(BackendEvent::CommandReply {
                id: outer.get(),
                error: 0,
            });
            owner.wait_for_ack().unwrap();
            assert_eq!(
                owner
                    .take_stopped()
                    .unwrap()
                    .unwrap()
                    .outcome
                    .unwrap_err()
                    .code,
                "pause_property"
            );
            driver.destroyed.recv().unwrap();
        }
    }

    #[test]
    fn thousands_of_gains_coalesce_before_surface_and_never_use_discrete_capacity() {
        let (driver, backend) = Driver::pair(Config::default());
        let mut owner = OwnerEndpoint::spawn_with_backend(generation(), move || backend).unwrap();
        let latest = PlaybackGain::new(80, true).unwrap();
        for index in 0..5_000 {
            let gain = PlaybackGain::new((index % 101) as u8, index % 2 == 0).unwrap();
            assert_eq!(
                owner.submit(generation(), PlaybackIntent::SetGain(gain)),
                SubmitStatus::Accepted
            );
        }
        assert_eq!(
            owner.submit(generation(), PlaybackIntent::SetGain(latest)),
            SubmitStatus::Accepted
        );
        assert!(driver.initialized.try_recv().is_err());
        assert_eq!(owner.attach(token()), SubmitStatus::Accepted);
        driver.initialized.recv().unwrap();
        let load = load(&driver);
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        let (gain_id, command) = driver.submitted.recv().unwrap();
        assert_eq!(command, BackendCommand::SetGain(latest));
        driver.send(BackendEvent::CommandReply {
            id: gain_id.get(),
            error: 0,
        });
        driver.fence();
        assert!(
            driver.submitted.try_recv().is_err(),
            "only the final preference is dispatched"
        );
        assert!(stop(&mut owner, &driver).outcome.is_ok());
    }

    #[test]
    fn gain_transaction_is_not_interleaved_and_discrete_pause_precedes_latest_pending_gain() {
        let (mut owner, driver) = start(Config::default());
        let load = load(&driver);
        let first = PlaybackGain::new(81, true).unwrap();
        owner.submit(generation(), PlaybackIntent::SetGain(first));
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        let (in_flight, command) = driver.submitted.recv().unwrap();
        assert_eq!(command, BackendCommand::SetGain(first));
        for index in 0..5_000 {
            assert_eq!(
                owner.submit(
                    generation(),
                    PlaybackIntent::SetGain(PlaybackGain::new((index % 101) as u8, false).unwrap())
                ),
                SubmitStatus::Accepted
            );
        }
        let latest = PlaybackGain::new(82, false).unwrap();
        owner.submit(generation(), PlaybackIntent::SetGain(latest));
        owner.submit(generation(), pause_intent());
        driver.fence();
        assert!(
            driver.submitted.try_recv().is_err(),
            "volume/mute outer transaction remains in flight"
        );
        driver.send(BackendEvent::CommandReply {
            id: in_flight.get(),
            error: 0,
        });
        let (pause_id, command) = driver.submitted.recv().unwrap();
        assert_eq!(command, pause_command());
        driver.send(BackendEvent::PauseObserved(PauseObservation {
            request: Some(PauseRequestId::new(1).unwrap()),
            paused: true,
        }));
        driver.send(BackendEvent::CommandReply {
            id: pause_id.get(),
            error: 0,
        });
        let (last_id, command) = driver.submitted.recv().unwrap();
        assert_eq!(command, BackendCommand::SetGain(latest));
        driver.send(BackendEvent::CommandReply {
            id: last_id.get(),
            error: 0,
        });
        driver.fence();
        assert!(driver.submitted.try_recv().is_err());
        assert!(stop(&mut owner, &driver).outcome.is_ok());
    }

    #[test]
    fn stop_wins_over_gain_mailbox_while_load_or_gain_transaction_is_pending() {
        for gain_pending in [false, true] {
            let (mut owner, driver) = start(Config::default());
            let load = load(&driver);
            owner.submit(
                generation(),
                PlaybackIntent::SetGain(PlaybackGain::new(80, true).unwrap()),
            );
            if gain_pending {
                driver.send(BackendEvent::CommandReply {
                    id: load.get(),
                    error: 0,
                });
                assert!(matches!(
                    driver.submitted.recv().unwrap().1,
                    BackendCommand::SetGain(_)
                ));
            }
            owner.submit(
                generation(),
                PlaybackIntent::SetGain(PlaybackGain::default()),
            );
            assert!(stop(&mut owner, &driver).outcome.is_ok());
            assert!(driver.submitted.try_recv().is_err());
            assert_eq!(
                owner.submit(
                    generation(),
                    PlaybackIntent::SetGain(PlaybackGain::default())
                ),
                SubmitStatus::Closing
            );
        }
    }

    #[test]
    fn bounded_commands_do_not_block_priority_stop_or_accept_a_sixty_fifth_slot() {
        let (mut owner, driver) = start(Config::default());
        load(&driver);
        for _ in 0..64 {
            assert_eq!(
                owner.submit(generation(), pause_intent()),
                SubmitStatus::Accepted
            );
        }
        assert_eq!(
            owner.submit(generation(), pause_intent()),
            SubmitStatus::CapacityExceeded
        );
        owner.wait_for_ack().unwrap();
        assert!(owner.take_stopped().unwrap().unwrap().outcome.is_err());
        driver.destroyed.recv().unwrap();
        assert!(driver.submitted.try_recv().is_err());
    }

    #[test]
    fn accepted_commands_remain_fifo_with_one_outstanding_request_and_unmatched_reply_does_not_clear_it()
     {
        let (mut owner, driver) = start(Config::default());
        let initial = load(&driver);
        owner.submit(generation(), pause_intent());
        owner.submit(generation(), pause_intent());
        driver.send(BackendEvent::CommandReply {
            id: initial.get() + 17,
            error: 0,
        });
        driver.fence();
        assert!(driver.submitted.try_recv().is_err());
        driver.send(BackendEvent::CommandReply {
            id: initial.get(),
            error: 0,
        });
        let (first, kind) = driver.submitted.recv().unwrap();
        assert_eq!(kind, pause_command());
        assert!(first.get() > initial.get());
        assert!(driver.submitted.try_recv().is_err());
        driver.send(BackendEvent::PauseObserved(PauseObservation {
            request: Some(PauseRequestId::new(1).unwrap()),
            paused: true,
        }));
        driver.send(BackendEvent::CommandReply {
            id: first.get(),
            error: 0,
        });
        let (second, kind) = driver.submitted.recv().unwrap();
        assert_eq!(kind, pause_command());
        assert!(second.get() > first.get());
        stop(&mut owner, &driver);
    }

    #[test]
    fn initialization_failures_with_and_without_handle_ack_only_after_shutdown() {
        for (boundary, handle) in [
            ("load", false),
            ("symbol", false),
            ("option", true),
            ("initialize", true),
        ] {
            let (mut owner, driver) = start(Config {
                initialization_error: Some(MediaError::new(boundary, boundary)),
                creates_handle: handle,
                hold_shutdown: true,
                ..Config::default()
            });
            assert!(owner.take_stopped().unwrap().is_none());
            assert!(driver.submitted.try_recv().is_err());
            driver.shutdown_release.send(()).unwrap();
            owner.wait_for_ack().unwrap();
            assert_eq!(driver.destroyed.recv().unwrap(), handle);
            assert!(owner.take_stopped().unwrap().unwrap().outcome.is_err());
            assert!(driver.destroyed.try_recv().is_err());
        }
    }

    #[test]
    fn cancellation_winning_initialize_submits_no_load() {
        let (mut owner, driver) = start(Config {
            hold_initialize: true,
            ..Config::default()
        });
        owner.stop(generation(), None);
        driver.initialize_release.send(()).unwrap();
        owner.wait_for_ack().unwrap();
        assert!(owner.take_stopped().unwrap().unwrap().outcome.is_ok());
        assert!(driver.submitted.try_recv().is_err());
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn negative_async_submission_never_waits_for_nonexistent_reply() {
        let (mut owner, driver) = start(Config {
            submission_error: Some(MediaError::new("submission", "negative async result")),
            ..Config::default()
        });
        owner.wait_for_ack().unwrap();
        assert!(owner.take_stopped().unwrap().unwrap().outcome.is_err());
        assert!(driver.submitted.try_recv().is_err());
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn negative_matching_reply_stops_and_does_not_dispatch_queued_intent() {
        let (mut owner, driver) = start(Config::default());
        let initial = load(&driver);
        owner.submit(generation(), pause_intent());
        driver.send(BackendEvent::CommandReply {
            id: initial.get(),
            error: -13,
        });
        owner.wait_for_ack().unwrap();
        assert!(owner.take_stopped().unwrap().unwrap().outcome.is_err());
        driver.destroyed.recv().unwrap();
        assert!(driver.submitted.try_recv().is_err());
    }

    #[test]
    fn event_queue_overflow_is_fatal_during_opening_and_ready() {
        for ready in [false, true] {
            let (mut owner, driver) = start(Config::default());
            load(&driver);
            if ready {
                driver.send(BackendEvent::FileLoaded);
                driver.fence();
            }
            driver.send(BackendEvent::QueueOverflow);
            owner.wait_for_ack().unwrap();
            assert_eq!(
                owner
                    .take_stopped()
                    .unwrap()
                    .unwrap()
                    .outcome
                    .unwrap_err()
                    .code,
                "event_overflow"
            );
            driver.destroyed.recv().unwrap();
        }
    }

    #[test]
    fn cancellation_bypasses_pending_load_and_pause_reply() {
        for pause in [false, true] {
            let (mut owner, driver) = start(Config::default());
            let initial = load(&driver);
            if pause {
                owner.submit(generation(), pause_intent());
                driver.send(BackendEvent::CommandReply {
                    id: initial.get(),
                    error: 0,
                });
                assert_eq!(driver.submitted.recv().unwrap().1, pause_command());
            }
            assert!(stop(&mut owner, &driver).outcome.is_ok());
        }
    }

    #[test]
    fn request_id_exhaustion_fails_without_reusing_identifier() {
        let (driver, backend) = Driver::pair(Config::default());
        let mut owner =
            OwnerEndpoint::spawn_with_backend_seed(generation(), move || backend, u64::MAX)
                .unwrap();
        owner.attach(token());
        driver.initialized.recv().unwrap();
        owner.wait_for_ack().unwrap();
        assert_eq!(
            owner
                .take_stopped()
                .unwrap()
                .unwrap()
                .outcome
                .unwrap_err()
                .code,
            "request_exhausted"
        );
        assert!(driver.submitted.try_recv().is_err());
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn pause_property_failure_is_fatal_on_file_loaded_and_pause_reply() {
        for on_load in [true, false] {
            let (mut owner, driver) = start(Config {
                pause_error: true,
                ..Config::default()
            });
            let initial = load(&driver);
            if on_load {
                driver.send(BackendEvent::FileLoaded);
            } else {
                owner.submit(generation(), pause_intent());
                driver.send(BackendEvent::CommandReply {
                    id: initial.get(),
                    error: 0,
                });
                let (pause, _) = driver.submitted.recv().unwrap();
                driver.send(BackendEvent::CommandReply {
                    id: pause.get(),
                    error: 0,
                });
            }
            owner.wait_for_ack().unwrap();
            assert_eq!(
                owner
                    .take_stopped()
                    .unwrap()
                    .unwrap()
                    .outcome
                    .unwrap_err()
                    .code,
                "pause_property"
            );
            driver.destroyed.recv().unwrap();
        }
    }

    #[test]
    fn latest_snapshot_keeps_readiness_across_eof_and_ack_has_separate_capacity() {
        let (mut owner, driver) = start(Config::default());
        load(&driver);
        driver.send(BackendEvent::FileLoaded);
        for _ in 0..4096 {
            driver.send(BackendEvent::EndFile {
                reason: 0,
                error: 0,
            });
        }
        driver.fence();
        let snapshot = owner.take_snapshot().unwrap();
        assert!(snapshot.file_loaded && snapshot.stream_ended == Some((0, 0)));
        owner.stop(generation(), None);
        owner.wait_for_ack().unwrap();
        assert!(owner.take_stopped().unwrap().unwrap().outcome.is_ok());
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn stale_generation_cannot_attach_submit_or_set_cancel() {
        let (mut owner, driver) = start(Config::default());
        load(&driver);
        let stale = Generation::new(2).unwrap();
        assert_eq!(
            owner.submit(stale, pause_intent()),
            SubmitStatus::StaleGeneration
        );
        assert_eq!(
            owner.stop(stale, Some(MediaError::new("stale", "stale"))),
            SubmitStatus::StaleGeneration
        );
        assert_eq!(
            owner.attach(SurfaceToken {
                generation: stale,
                xid: token().xid
            }),
            SubmitStatus::StaleGeneration
        );
        driver.fence();
        assert!(owner.take_stopped().unwrap().is_none());
        assert!(stop(&mut owner, &driver).outcome.is_ok());
    }

    #[test]
    fn playback_error_and_shutdown_require_real_cleanup_not_end_event_ack() {
        for event in [
            BackendEvent::EndFile {
                reason: 4,
                error: -13,
            },
            BackendEvent::Shutdown,
        ] {
            let (mut owner, driver) = start(Config {
                hold_shutdown: true,
                ..Config::default()
            });
            load(&driver);
            driver.send(event);
            assert!(owner.take_stopped().unwrap().is_none());
            driver.shutdown_release.send(()).unwrap();
            owner.wait_for_ack().unwrap();
            assert!(owner.take_stopped().unwrap().unwrap().outcome.is_err());
            driver.destroyed.recv().unwrap();
        }
    }

    #[test]
    fn fatal_cancellation_dominates_clean_close_even_during_destruction() {
        let (mut owner, driver) = start(Config {
            hold_shutdown: true,
            ..Config::default()
        });
        load(&driver);
        assert_eq!(owner.stop(generation(), None), SubmitStatus::Accepted);
        assert_eq!(
            owner.stop(
                generation(),
                Some(MediaError::new("surface_lost", "lost while stopping"))
            ),
            SubmitStatus::Accepted
        );
        driver.shutdown_release.send(()).unwrap();
        owner.wait_for_ack().unwrap();
        assert_eq!(
            owner
                .take_stopped()
                .unwrap()
                .unwrap()
                .outcome
                .unwrap_err()
                .code,
            "surface_lost"
        );
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn owner_disconnect_is_failure_not_fabricated_destruction_ack() {
        let mut owner = OwnerEndpoint::spawn_with_backend(
            generation(),
            || -> super::test_support::FakeBackend {
                panic!("injected owner factory panic");
            },
        )
        .unwrap();
        assert_eq!(
            wait_for_owner_disconnect(&mut owner).code,
            "owner_disconnect"
        );
        assert!(!owner.ack_received);
        // The worker-join panic path is sticky: a repeat call must remain an
        // owner_disconnect error, never a completed destruction ack.
        assert_eq!(owner.take_stopped().unwrap_err().code, "owner_disconnect");
        assert!(!owner.ack_received);
    }

    #[test]
    fn surface_loss_barrier_waits_for_destruction_and_preserves_single_consumption() {
        let (mut owner, driver) = start(Config {
            creates_handle: true,
            hold_shutdown: true,
            ..Config::default()
        });
        load(&driver);
        owner.stop(
            generation(),
            Some(MediaError::new("surface_lost", "lost while stopping")),
        );
        assert!(owner.take_stopped().unwrap().is_none());
        assert!(driver.destroyed.try_recv().is_err());

        let (started_tx, started_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel::<Result<(), MediaError>>();
        let destroyed = driver.destroyed;
        let waiter = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = owner.wait_for_ack();
            let destruction = destroyed.try_recv();
            completed_tx.send(result).unwrap();
            (owner, destruction)
        });
        started_rx.recv().unwrap();
        let while_held = completed_rx.recv_timeout(Duration::from_millis(50));
        // Release before assertions so a failed negative check cannot strand the owner.
        driver.shutdown_release.send(()).unwrap();
        let completion = completed_rx.recv().unwrap();
        let (mut owner, destruction) = waiter.join().unwrap();
        assert_eq!(while_held, Err(mpsc::RecvTimeoutError::Timeout));
        completion.unwrap();
        assert!(destruction.unwrap());

        owner.wait_for_ack().unwrap();
        owner.wait_for_ack().unwrap();
        let stopped = owner.take_stopped().unwrap().unwrap();
        assert_eq!(stopped.generation, generation());
        assert_eq!(stopped.outcome.unwrap_err().code, "surface_lost");
        owner.wait_for_ack().unwrap();
        assert!(owner.take_stopped().unwrap().is_none());
    }

    #[test]
    fn surface_loss_barrier_owner_disconnect_returns_error_without_fabricated_ack() {
        for disconnect_already_observed in [false, true] {
            let mut owner = OwnerEndpoint::spawn_with_backend(
                generation(),
                || -> super::test_support::FakeBackend {
                    panic!("injected owner factory panic");
                },
            )
            .unwrap();
            if disconnect_already_observed {
                assert_eq!(
                    wait_for_owner_disconnect(&mut owner).code,
                    "owner_disconnect"
                );
            }
            // The join failure surfaces as an owner_disconnect failure; the
            // wording belongs to the join path, so only the code is asserted.
            let first = owner.wait_for_ack().unwrap_err();
            assert_eq!(first.code, "owner_disconnect");
            let repeat = owner.wait_for_ack().unwrap_err();
            assert_eq!(repeat.code, "owner_disconnect");
            assert!(!owner.ack_received);
            assert!(owner.buffered_ack.is_none());
            assert!(owner.take_stopped().unwrap().is_none());
        }
    }
}

#[cfg(test)]
mod audio_tests {
    use super::test_support::{Config, Driver};
    use super::tests::{generation, load, start};
    use super::*;

    #[test]
    fn stop_sets_independent_audio_latch_during_blocked_initialization() {
        let generation = Generation::new(1).unwrap();
        let (driver, backend) = Driver::pair(Config {
            hold_initialize: true,
            ..Config::default()
        });
        let mut owner = OwnerEndpoint::spawn_with_backend(generation, move || backend).unwrap();
        let latch = owner.stop_flag.audio_cancel();
        owner.attach(SurfaceToken {
            generation,
            xid: X11WindowId::new(47).unwrap(),
        });
        driver.initialized.recv().unwrap();
        assert!(!latch.load(Ordering::Acquire));
        owner.stop(generation, None);
        assert!(latch.load(Ordering::Acquire));
        driver.initialize_release.send(()).unwrap();
        owner.wait_for_ack().unwrap();
        assert!(owner.take_stopped().unwrap().unwrap().outcome.is_ok());
        assert!(driver.submitted.try_recv().is_err());
    }

    #[test]
    fn stale_gain_does_not_dispatch_or_cancel_current_generation() {
        let generation = Generation::new(1).unwrap();
        let (driver, backend) = Driver::pair(Config::default());
        let mut owner = OwnerEndpoint::spawn_with_backend(generation, move || backend).unwrap();
        owner.attach(SurfaceToken {
            generation,
            xid: X11WindowId::new(47).unwrap(),
        });
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        assert_eq!(
            owner.submit(
                Generation::new(2).unwrap(),
                PlaybackIntent::SetGain(PlaybackGain::default())
            ),
            SubmitStatus::StaleGeneration
        );
        assert!(!owner.stop_flag.audio_cancel().load(Ordering::Acquire));
        assert!(driver.submitted.try_recv().is_err());
        owner.stop(generation, None);
        owner.wait_for_ack().unwrap();
    }

    #[test]
    fn latest_output_is_coalesced_and_bypasses_pending_video_load_without_wrong_generation() {
        let (mut owner, driver) = start(Config {
            hold_initialize: true,
            ..Config::default()
        });
        let first = OutputPlan::Silent {
            revision: crate::domain::output::OutputRevision::first(),
            reason: crate::domain::output::OutputSilence::ManualUnavailable,
        };
        let latest = OutputPlan::Silent {
            revision: first.revision().next(),
            reason: crate::domain::output::OutputSilence::ManualRequiresAction,
        };
        assert_eq!(
            owner.submit(
                Generation::new(2).unwrap(),
                PlaybackIntent::SetOutput(first.clone())
            ),
            SubmitStatus::StaleGeneration
        );
        assert_eq!(
            owner.submit(generation(), PlaybackIntent::SetOutput(first)),
            SubmitStatus::Accepted
        );
        assert_eq!(
            owner.submit(generation(), PlaybackIntent::SetOutput(latest.clone())),
            SubmitStatus::Accepted
        );
        driver.initialize_release.send(()).unwrap();
        load(&driver); // Intentionally withhold its terminal reply.
        let (_, command) = driver.submitted.recv().unwrap();
        assert_eq!(command, BackendCommand::SetOutput(latest));
        assert!(driver.submitted.try_recv().is_err());
        owner.stop(generation(), None);
        owner.wait_for_ack().unwrap();
        assert!(owner.take_stopped().unwrap().unwrap().outcome.is_ok());
    }

    #[test]
    fn audio_retirement_precedes_video_destroy_and_joined_session_ack() {
        let generation = Generation::new(1).unwrap();
        let (driver, backend) = Driver::pair(Config {
            creates_handle: true,
            hold_quiesce: true,
            ..Config::default()
        });
        let mut owner = OwnerEndpoint::spawn_with_backend(generation, move || backend).unwrap();
        owner.attach(SurfaceToken {
            generation,
            xid: X11WindowId::new(47).unwrap(),
        });
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        owner.stop(generation, None);
        driver.quiesce_started.recv().unwrap();
        assert!(driver.destroyed.try_recv().is_err());
        assert!(owner.take_stopped().unwrap().is_none());
        driver.quiesce_release.send(()).unwrap();
        assert!(driver.destroyed.recv().unwrap());
        owner.wait_for_ack().unwrap();
        assert!(owner.take_stopped().unwrap().unwrap().outcome.is_ok());
    }

    #[test]
    fn source_loss_stays_audio_only_and_actual_stream_failure_is_correlated() {
        let generation = Generation::new(1).unwrap();
        let (driver, backend) = Driver::pair(Config::default());
        let mut owner = OwnerEndpoint::spawn_with_backend(generation, move || backend).unwrap();
        owner.attach(SurfaceToken {
            generation,
            xid: X11WindowId::new(47).unwrap(),
        });
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        let failure = AudioError::SourceMissing {
            name: "exact-selected-source".into(),
        };
        driver.send(BackendEvent::AudioAvailability(AudioAvailability::Silent {
            reason: crate::domain::capture::AudioSilence::WaitingForSource(failure.clone()),
        }));
        driver.send(BackendEvent::FileLoaded);
        driver.fence();
        let snapshot = owner.take_snapshot().unwrap();
        assert!(snapshot.file_loaded);
        assert_eq!(
            snapshot.audio,
            AudioAvailability::Silent {
                reason: crate::domain::capture::AudioSilence::WaitingForSource(failure),
            }
        );
        assert!(snapshot.failure.is_none());
        driver.send(BackendEvent::EndFile {
            reason: 4,
            error: -13,
        });
        owner.wait_for_ack().unwrap();
        assert_eq!(
            owner
                .take_stopped()
                .unwrap()
                .unwrap()
                .outcome
                .unwrap_err()
                .code,
            "playback"
        );
    }
}

#[cfg(test)]
mod recovery_readiness_tests {
    use super::*;
    use crate::domain::capture::{CaptureMode, CapturedFourCc, FrameRate, FrameSize};

    fn paused_snapshot() -> Snapshot {
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(2560, 1440).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        let catalog = crate::capture::linux::session_fixture(&["/dev/video0"], mode);
        let selection =
            CaptureSelection::from_snapshot(&catalog, std::path::Path::new("/dev/video0"), mode)
                .unwrap();
        let input = selection.validate_snapshot(&catalog).unwrap();
        Snapshot {
            generation: Generation::new(1).unwrap(),
            initialized: true,
            file_loaded: true,
            playback_started: false,
            session: Some(ReadinessFacts::Capture(
                SessionFacts::verify(input.requested().clone(), ObservedFacts::default()).unwrap(),
            )),
            pause: Some(PauseObservation {
                request: None,
                paused: true,
            }),
            pause_pending: false,
            stream_ended: None,
            failure: None,
            audio: AudioAvailability::Disabled,
            readiness: None,
            load_complete: false,
            playback: InitialPlayback::Paused,
            audio_detached: None,
            audio_epoch: None,
            treatments: None,
            treatment_fault: None,
            diagnostics: None,
        }
    }

    #[test]
    fn prepared_paused_needs_load_terminal_and_real_pause_but_not_playback_restart() {
        let mut snapshot = paused_snapshot();
        update_readiness(&mut snapshot);
        assert_eq!(snapshot.readiness, None);
        snapshot.load_complete = true;
        update_readiness(&mut snapshot);
        assert_eq!(snapshot.readiness, Some(OpenReadiness::PausedPrepared));
        assert!(!snapshot.playback_started);
        snapshot.pause = None;
        update_readiness(&mut snapshot);
        assert_eq!(snapshot.readiness, None);
        snapshot.pause = Some(PauseObservation {
            request: None,
            paused: false,
        });
        update_readiness(&mut snapshot);
        assert_eq!(snapshot.readiness, None);
    }

    #[test]
    fn paused_metadata_never_claims_live_and_terminal_health_revokes_prepared_readiness() {
        let mut snapshot = paused_snapshot();
        snapshot.load_complete = true;
        snapshot.playback = InitialPlayback::Live;
        snapshot.pause = Some(PauseObservation {
            request: None,
            paused: false,
        });
        update_readiness(&mut snapshot);
        assert_eq!(snapshot.readiness, None);
        snapshot.playback_started = true;
        update_readiness(&mut snapshot);
        assert_eq!(snapshot.readiness, Some(OpenReadiness::Live));
        snapshot.stream_ended = Some((4, -13));
        update_readiness(&mut snapshot);
        assert_eq!(snapshot.readiness, None);
        assert_eq!(snapshot.stream_ended, Some((4, -13)));
        snapshot.stream_ended = None;
        snapshot.failure = Some(MediaError::new("capture_metadata", "contradictory format"));
        update_readiness(&mut snapshot);
        assert_eq!(snapshot.readiness, None);
    }

    #[test]
    fn startup_readiness_expires_at_exact_budget_with_honest_structured_error() {
        use crate::domain::failure::{Cause, Stage};
        let accepted = Instant::now();
        for (elapsed, expired) in [
            (OPEN_READINESS_DEADLINE - Duration::from_nanos(1), false),
            (OPEN_READINESS_DEADLINE, true),
            (OPEN_READINESS_DEADLINE + Duration::from_nanos(1), true),
        ] {
            let mut snapshot = paused_snapshot();
            let requested = snapshot
                .session
                .as_ref()
                .unwrap()
                .capture()
                .unwrap()
                .requested
                .clone();
            let mut started = Some(accepted);
            let result = check_startup_readiness(
                &mut snapshot,
                true,
                &StopFlag::new(),
                &mut started,
                Some(&requested),
                accepted + elapsed,
            );
            assert_eq!(result.is_err(), expired);
            assert_eq!(started, Some(accepted));
            assert!(snapshot.readiness.is_none());
            assert!(snapshot.treatments.is_none() && snapshot.treatment_fault.is_none());
            if let Err(error) = result {
                assert_eq!(error.code, "startup_readiness_timeout");
                assert_eq!(
                    error.diagnostic,
                    "native playback readiness was not established within 10 seconds"
                );
                let session = error.session.unwrap();
                assert_eq!(session.requested, requested);
                assert_eq!(
                    (session.stage, session.cause),
                    (Stage::Unknown, Cause::Generic)
                );
                assert_eq!(session.diagnostic, error.diagnostic);
                assert!(session.evidence.is_none() && session.source.is_none());
            }
        }
        let mut snapshot = paused_snapshot();
        let error = check_startup_readiness(
            &mut snapshot,
            true,
            &StopFlag::new(),
            &mut Some(accepted),
            None,
            accepted + OPEN_READINESS_DEADLINE,
        )
        .unwrap_err();
        assert_eq!(error.code, "startup_readiness_timeout");
        assert!(error.session.is_none());
    }

    fn live_snapshot() -> Snapshot {
        let mut snapshot = paused_snapshot();
        snapshot.load_complete = true;
        snapshot.playback = InitialPlayback::Live;
        snapshot.playback_started = true;
        snapshot.pause.as_mut().unwrap().paused = false;
        snapshot
    }

    #[test]
    fn silent_missing_facts_and_nonclean_flood_cannot_escape_initial_budget() {
        let accepted = Instant::now();
        for missing in 0..6 {
            let mut snapshot = live_snapshot();
            match missing {
                0 => snapshot.load_complete = false,
                1 => snapshot.file_loaded = false,
                2 => snapshot.session = None,
                3 => snapshot.pause = None,
                4 => snapshot.playback_started = false,
                5 => {}
                _ => unreachable!(),
            }
            let drained = missing != 5;
            let mut started = Some(accepted);
            for elapsed in [0, 1000, 9900, 9999] {
                check_startup_readiness(
                    &mut snapshot,
                    drained,
                    &StopFlag::new(),
                    &mut started,
                    None,
                    accepted + Duration::from_millis(elapsed),
                )
                .unwrap();
                assert!(snapshot.readiness.is_none());
                assert_eq!(started, Some(accepted));
            }
            let error = check_startup_readiness(
                &mut snapshot,
                drained,
                &StopFlag::new(),
                &mut started,
                None,
                accepted + OPEN_READINESS_DEADLINE,
            )
            .unwrap_err();
            assert_eq!(error.code, "startup_readiness_timeout");
        }
    }

    #[test]
    fn pre_readiness_metadata_pause_and_diagnostic_activity_never_resets_budget() {
        let accepted = Instant::now();
        let mut snapshot = live_snapshot();
        snapshot.playback_started = false;
        let mut started = Some(accepted);
        for epoch in 1..=10 {
            snapshot.pause.as_mut().unwrap().paused = epoch % 2 == 0;
            snapshot.load_complete = epoch % 3 != 0;
            snapshot.diagnostics = Some(RuntimeMediaObservation {
                generation: snapshot.generation.get() + epoch % 2,
                observation_epoch: epoch,
                client_api: 0x0002_0005,
                libmpv_version: "mpv v0.41.0".into(),
                backend: BackendObservation::Unavailable(BackendUnavailable::NotConfigured),
                helpers: Default::default(),
            });
            let result = check_startup_readiness(
                &mut snapshot,
                epoch % 2 == 0,
                &StopFlag::new(),
                &mut started,
                None,
                accepted + Duration::from_secs(epoch),
            );
            assert_eq!(result.is_err(), epoch == 10);
            assert_eq!(started, Some(accepted));
            assert!(snapshot.readiness.is_none());
        }
    }

    #[test]
    fn timely_live_and_paused_preparation_disarm_forever_despite_later_pause_work() {
        let accepted = Instant::now();
        let mut paused = paused_snapshot();
        paused.load_complete = true;
        for (mut snapshot, expected) in [
            (live_snapshot(), OpenReadiness::Live),
            (paused, OpenReadiness::PausedPrepared),
        ] {
            let mut started = Some(accepted);
            check_startup_readiness(
                &mut snapshot,
                true,
                &StopFlag::new(),
                &mut started,
                None,
                accepted + OPEN_READINESS_DEADLINE - Duration::from_nanos(1),
            )
            .unwrap();
            assert_eq!(snapshot.readiness, Some(expected));
            assert_eq!(started, None);
            if expected == OpenReadiness::PausedPrepared {
                assert!(!snapshot.playback_started);
            }
            snapshot.pause = None;
            for drained in [false, true, false] {
                check_startup_readiness(
                    &mut snapshot,
                    drained,
                    &StopFlag::new(),
                    &mut started,
                    None,
                    accepted + Duration::from_secs(60),
                )
                .unwrap();
                assert_eq!(started, None);
            }
            assert!(snapshot.readiness.is_none());
        }
    }

    #[test]
    fn first_readiness_at_or_after_deadline_cannot_disarm_expired_budget() {
        let accepted = Instant::now();
        for elapsed in [
            OPEN_READINESS_DEADLINE,
            OPEN_READINESS_DEADLINE + Duration::from_nanos(1),
        ] {
            for readiness in [OpenReadiness::Live, OpenReadiness::PausedPrepared] {
                let mut snapshot = live_snapshot();
                snapshot.readiness = Some(readiness);
                let mut started = Some(accepted);
                let error = check_startup_readiness(
                    &mut snapshot,
                    true,
                    &StopFlag::new(),
                    &mut started,
                    None,
                    accepted + elapsed,
                )
                .unwrap_err();
                assert_eq!(error.code, "startup_readiness_timeout");
                assert_eq!(started, Some(accepted));
            }
        }
    }

    #[test]
    fn cancellation_and_observed_terminal_keep_priority_over_startup_expiry() {
        let accepted = Instant::now();
        for reason in [
            None,
            Some(MediaError::new("surface_lost", "surface was lost")),
        ] {
            let stop = StopFlag::new();
            stop.request(reason.clone());
            let mut snapshot = live_snapshot();
            let result = check_startup_readiness(
                &mut snapshot,
                true,
                &stop,
                &mut Some(accepted),
                None,
                accepted + OPEN_READINESS_DEADLINE,
            );
            assert_eq!(result, reason.map_or(Ok(()), Err));
            assert!(snapshot.readiness.is_none());
        }
        let mut snapshot = live_snapshot();
        snapshot.stream_ended = Some((0, 0));
        check_startup_readiness(
            &mut snapshot,
            true,
            &StopFlag::new(),
            &mut Some(accepted),
            None,
            accepted + OPEN_READINESS_DEADLINE,
        )
        .unwrap();
        assert_eq!(snapshot.stream_ended, Some((0, 0)));
        assert!(snapshot.readiness.is_none());
    }
    #[test]
    fn qualification_requires_real_decoded_dimensions_and_never_becomes_capture_facts() {
        let expected = FrameSize::new(320, 180).unwrap();
        let input = OwnerInput::QualificationVideo(expected);
        let mut snapshot = live_snapshot();
        snapshot.session = None;
        apply_observed(&mut snapshot, &input, true, ObservedFacts::default()).unwrap();
        update_readiness(&mut snapshot);
        assert!(snapshot.readiness.is_none());
        snapshot.session = Some(ReadinessFacts::QualificationVideo(ObservedFacts::default()));
        update_readiness(&mut snapshot);
        assert!(
            snapshot.readiness.is_none(),
            "an empty wrapper is not decoder evidence"
        );
        let observed = ObservedFacts {
            decoded_size: Some(super::super::session::Observation {
                value: expected,
                source: super::super::session::Source::MpvDecodedParams,
            }),
            ..Default::default()
        };
        apply_observed(&mut snapshot, &input, true, observed).unwrap();
        update_readiness(&mut snapshot);
        assert_eq!(snapshot.readiness, Some(OpenReadiness::Live));
        assert!(snapshot.session.as_ref().unwrap().capture().is_none());
        snapshot.pause = Some(PauseObservation {
            request: Some(PauseRequestId::new(1).unwrap()),
            paused: true,
        });
        update_readiness(&mut snapshot);
        assert!(snapshot.readiness.is_none());
        let wrong = ObservedFacts {
            decoded_size: Some(super::super::session::Observation {
                value: FrameSize::new(640, 360).unwrap(),
                source: super::super::session::Source::MpvDecodedParams,
            }),
            ..Default::default()
        };
        assert_eq!(
            apply_observed(&mut snapshot, &input, true, wrong)
                .unwrap_err()
                .code,
            "qualification_format"
        );
    }
    #[test]
    fn qualification_missing_decoder_evidence_expires_without_capture_identity() {
        let mut snapshot = live_snapshot();
        snapshot.session = Some(ReadinessFacts::QualificationVideo(ObservedFacts::default()));
        let accepted = Instant::now();
        let mut started = Some(accepted);
        assert!(
            check_startup_readiness(
                &mut snapshot,
                true,
                &StopFlag::new(),
                &mut started,
                None,
                accepted + OPEN_READINESS_DEADLINE - Duration::from_nanos(1)
            )
            .is_ok()
        );
        assert!(snapshot.readiness.is_none());
        let error = check_startup_readiness(
            &mut snapshot,
            true,
            &StopFlag::new(),
            &mut started,
            None,
            accepted + OPEN_READINESS_DEADLINE,
        )
        .unwrap_err();
        assert_eq!(error.code, "startup_readiness_timeout");
        assert!(error.session.is_none());
    }
}

#[cfg(test)]
mod diagnostics_emission_tests {
    use super::*;
    use crate::domain::capture::{CaptureMode, CapturedFourCc, FrameRate, FrameSize};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::{Layer, layer::Context, prelude::*};

    /// Records exactly the fields an emitter hands to the tracing boundary, so
    /// a re-introduced private value fails without relying on the sink layer.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<(String, String)>>>);

    struct Recorded<'a>(&'a mut Vec<(String, String)>);
    impl Visit for Recorded<'_> {
        fn record_debug(&mut self, field: &Field, _: &dyn std::fmt::Debug) {
            self.0.push((
                field.name().to_string(),
                format!("<debug:{}>", field.name()),
            ));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.push((field.name().to_string(), value.to_string()));
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.push((field.name().to_string(), value.to_string()));
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.0.push((field.name().to_string(), value.to_string()));
        }
        fn record_f64(&mut self, field: &Field, value: f64) {
            self.0.push((field.name().to_string(), value.to_string()));
        }
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.0.push((field.name().to_string(), value.to_string()));
        }
    }
    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            let mut fields = Vec::new();
            event.record(&mut Recorded(&mut fields));
            self.0.lock().extend(fields);
        }
    }

    #[test]
    fn capture_emissions_publish_only_bounded_scalars_never_the_device_node() {
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(2560, 1440).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        let catalog = crate::capture::linux::session_fixture(&["/dev/video7"], mode);
        let selection =
            CaptureSelection::from_snapshot(&catalog, std::path::Path::new("/dev/video7"), mode)
                .unwrap();
        let input = selection.validate_snapshot(&catalog).unwrap();
        let requested = input.requested().clone();

        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            log_capture_requested(Generation::new(4).unwrap(), &requested);
        });
        let fields = capture.0.lock().clone();
        // The exact device node and any raw Debug/Display value must never cross
        // the emitter boundary; only bounded scalars remain.
        assert!(fields.iter().all(|(_, value)| !value.contains("/dev/")));
        assert!(
            fields
                .iter()
                .all(|(_, value)| !value.starts_with("<debug:"))
        );
        assert!(fields.iter().all(|(name, _)| !matches!(
            name.as_str(),
            "node" | "path" | "requested" | "serial" | "topology"
        )));
        assert!(
            fields
                .iter()
                .any(|(name, value)| name == "event_code" && value == "capture_requested")
        );
        assert!(
            fields
                .iter()
                .any(|(name, value)| name == "width" && value == "2560")
        );
        assert!(fields.iter().any(|(name, _)| name == "fourcc"));
        assert!(fields.iter().any(|(name, _)| name == "generation"));
    }
}
