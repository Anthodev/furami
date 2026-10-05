//! One owner thread, bounded intents, coalesced progress and a destruction-only ack.

use std::{
    num::{NonZeroU32, NonZeroU64},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, Thread},
    time::Duration,
};

use super::session::{ObservedFacts, RequestedFacts, SessionError, SessionFacts};
use crate::app::ports::OpenReadiness;
#[cfg(test)]
use crate::capture::input::InputSpec;
use crate::capture::input::{CaptureSelection, SelectionError};
use crate::domain::capture::{
    AudioAvailability, AudioEpoch, AudioError, AudioSelection, AudioSourceIdentity, PlaybackGain,
    SelectedRouteAuthorization, WatchStamp,
};
use crate::domain::state::{InitialPlayback, PauseRequestId};

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub video: CaptureSelection,
    pub audio: AudioSelection,
    pub gain: PlaybackGain,
    pub playback: InitialPlayback,
    pub watch: WatchStamp,
    pub selected_route: Option<SelectedRouteAuthorization>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybackIntent {
    SetPaused {
        request: PauseRequestId,
        paused: bool,
    },
    SetGain(PlaybackGain),
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
pub(crate) struct Snapshot {
    pub generation: Generation,
    pub initialized: bool,
    pub file_loaded: bool,
    pub playback_started: bool,
    pub session: Option<SessionFacts>,
    pub pause: Option<PauseObservation>,
    pub stream_ended: Option<(i32, i32)>,
    pub failure: Option<MediaError>,
    pub audio: AudioAvailability,
    pub readiness: Option<OpenReadiness>,
    pub load_complete: bool,
    pub playback: InitialPlayback,
    pub audio_detached: Option<(AudioEpoch, Result<(), AudioError>)>,
    pub audio_epoch: Option<AudioEpoch>,
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
}
impl StopFlag {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
            reason: Mutex::new(None),
            audio_cancel: Arc::new(AtomicBool::new(false)),
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BackendCommand {
    LoadInput,
    SetPaused {
        request: PauseRequestId,
        paused: bool,
    },
    SetGain(PlaybackGain),
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
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BackendEvent {
    None,
    FileLoaded,
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
    fn diagnostic(&self) -> Option<String>;
    fn audio_status(&self) -> AudioAvailability {
        AudioAvailability::Disabled
    }
    fn initial_playback(&self) -> InitialPlayback {
        InitialPlayback::Live
    }
    fn audio_epoch(&self) -> Option<AudioEpoch> {
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
}
impl OwnerEndpoint {
    pub(crate) fn spawn(
        generation: Generation,
        prefix: String,
        config: SessionConfig,
    ) -> Result<Self, MediaError> {
        let requested = config.video.requested();
        let settings = crate::domain::state::DraftSettings {
            video: crate::domain::capture::ModeRequest {
                identity: requested.identity.clone(),
                mode: requested.mode,
            },
            audio: config.audio.clone(),
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
                tracing::info!(generation = generation.get(), requested = %requested, "capture_requested");
                run_backend(
                    super::ffi::MpvBackend::new(
                        prefix,
                        input,
                        config.audio,
                        config.gain,
                        config.playback,
                        config.watch,
                    ),
                    surface,
                    commands,
                    gain,
                    stop,
                    latest,
                    snapshot,
                    0,
                    Some(&requested),
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
                tracing::info!(generation = generation.get(), requested = %requested, "capture_requested");
                run_backend(
                    factory(input),
                    surface,
                    commands,
                    gain,
                    stop,
                    latest,
                    snapshot,
                    0,
                    Some(&requested),
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
                    Some(&requested),
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
                    None,
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
                    stream_ended: None,
                    failure: None,
                    audio: AudioAvailability::Disabled,
                    readiness: None,
                    load_complete: false,
                    playback: InitialPlayback::Live,
                    audio_detached: None,
                    audio_epoch: None,
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
        drop(handle);
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
    requested: Option<&RequestedFacts>,
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
            requested,
        ),
        None => Err(MediaError::new("owner_backend", "owner backend absent")),
    }
    .map_err(|error| match requested {
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
        generation = snapshot.generation.get(),
        "owner_destroy_begin"
    );
    if let Err(error) = backend.shutdown() {
        stop.request(Some(error.clone()));
        if outcome.is_ok() {
            snapshot.failure = Some(error.clone());
            publish(latest, snapshot);
            outcome = Err(error);
        }
    }
    tracing::info!(
        generation = snapshot.generation.get(),
        "owner_destroy_complete"
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
    requested: Option<&RequestedFacts>,
) -> Result<(), MediaError> {
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
    let mut pending = Some((id, InFlight::LoadInput));
    loop {
        if stop.is_set() {
            return stop.outcome();
        }
        if pending.is_none() && snapshot.stream_ended.is_none() {
            match commands.try_recv() {
                Ok(intent) => {
                    if stop.is_set() {
                        return stop.outcome();
                    }
                    let id = RequestId::next(&mut last_request)?;
                    let command = match intent {
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
                        PlaybackIntent::SetGain(_) => unreachable!("gain uses latest slot"),
                    };
                    if matches!(command, BackendCommand::SetPaused { .. }) {
                        snapshot.pause = None;
                    }
                    let in_flight = match &command {
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
                        apply_observed(snapshot, requested, false, observed)?;
                    }
                    publish(latest, snapshot);
                }
                BackendEvent::PlaybackRestart | BackendEvent::VideoReconfig => {
                    let started = matches!(event, BackendEvent::PlaybackRestart);
                    if let Some(observed) = backend.refresh_observed(started)? {
                        apply_observed(snapshot, requested, started, observed)?;
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
                    apply_observed(snapshot, requested, started, facts)?;
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
                BackendEvent::AudioDetached { epoch, outcome } => {
                    snapshot.audio_detached = Some((epoch, outcome));
                    publish(latest, snapshot);
                }
                BackendEvent::CommandReply { id, error } => {
                    if let Some((expected, kind)) = pending.as_ref()
                        && expected.get() == id
                    {
                        if error < 0 {
                            return Err(MediaError::new(
                                "command_reply",
                                format!(
                                    "mpv async command failed: {error}; {}",
                                    backend.diagnostic().unwrap_or_default()
                                ),
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
                    tracing::info!(
                        generation = snapshot.generation.get(),
                        reason,
                        error,
                        "mpv_end_file"
                    );
                    if stop.is_set() {
                        return stop.outcome();
                    }
                    snapshot.stream_ended = Some((reason, error));
                    snapshot.readiness = None;
                    publish(latest, snapshot);
                    if error < 0 || reason == 4 {
                        return Err(MediaError::new(
                            "playback",
                            format!(
                                "mpv END_FILE reason={reason} error={error}; {}",
                                backend.diagnostic().unwrap_or_default()
                            ),
                        ));
                    }
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
        if stop.is_set() {
            return stop.outcome();
        }
        let previous_readiness = snapshot.readiness;
        if drained {
            update_readiness(snapshot);
        }
        if snapshot.readiness != previous_readiness {
            publish(latest, snapshot);
        }
        if drained {
            thread::park_timeout(Duration::from_millis(10));
        }
    }
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
                generation = snapshot.generation.get(),
                request = request.get(),
                paused = observation.paused,
                "mpv_pause_observed"
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
    requested: Option<&RequestedFacts>,
    started: bool,
    observed: ObservedFacts,
) -> Result<(), MediaError> {
    if started || snapshot.playback_started || snapshot.playback == InitialPlayback::Paused {
        if let Some(requested) = requested {
            let facts = SessionFacts::verify(requested.clone(), observed)
                .map_err(MediaError::from_session)?;
            tracing::info!(generation = snapshot.generation.get(),
                facts = %serde_json::to_string(&facts).map_err(|error| MediaError::new("capture_metadata", error.to_string()))?,
                "capture_observed");
            snapshot.session = Some(facts);
        }
        snapshot.playback_started |= started;
    }
    Ok(())
}

fn update_readiness(snapshot: &mut Snapshot) {
    snapshot.readiness = if !snapshot.initialized
        || !snapshot.file_loaded
        || !snapshot.load_complete
        || snapshot.session.is_none()
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
        fn diagnostic(&self) -> Option<String> {
            None
        }

        fn shutdown(&mut self) -> Result<(), MediaError> {
            let _ = self.shutdown_started.send(());
            if self.config.hold_shutdown {
                self.shutdown_release.recv().unwrap();
            }
            self.destroyed.send(self.has_handle).unwrap();
            self.has_handle = false;
            if self.config.hold_quiesce {
                self.quiesce_started.send(()).unwrap();
                self.quiesce_release.recv().unwrap();
            }
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
    fn generation() -> Generation {
        Generation::new(1).unwrap()
    }
    fn token() -> SurfaceToken {
        SurfaceToken {
            generation: generation(),
            xid: X11WindowId::new(47).unwrap(),
        }
    }
    fn start(config: Config) -> (OwnerEndpoint, Driver) {
        let (driver, backend) = Driver::pair(config);
        let owner = OwnerEndpoint::spawn_with_backend(generation(), move || backend).unwrap();
        assert_eq!(owner.attach(token()), SubmitStatus::Accepted);
        driver.initialized.recv().unwrap();
        (owner, driver)
    }
    fn load(driver: &Driver) -> RequestId {
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
        assert!(owner.stopped.recv().is_err());
        assert_eq!(owner.take_stopped().unwrap_err().code, "owner_disconnect");
        assert!(!owner.ack_received);
        assert!(owner.take_stopped().unwrap().is_none());
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
                assert!(owner.stopped.recv().is_err());
                assert_eq!(owner.take_stopped().unwrap_err().code, "owner_disconnect");
            }
            let expected = MediaError::new(
                "owner_disconnect",
                "owner exited without destruction completion acknowledgment",
            );
            assert_eq!(owner.wait_for_ack().unwrap_err(), expected);
            assert_eq!(owner.wait_for_ack().unwrap_err(), expected);
            assert!(!owner.ack_received);
            assert!(owner.buffered_ack.is_none());
            assert!(owner.take_stopped().unwrap().is_none());
        }
    }
}

#[cfg(test)]
mod audio_tests {
    use super::test_support::{Config, Driver};
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
    fn destruction_ack_waits_for_audio_worker_quiescence_after_native_destroy() {
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
        assert!(driver.destroyed.recv().unwrap());
        driver.quiesce_started.recv().unwrap();
        assert!(owner.take_stopped().unwrap().is_none());
        driver.quiesce_release.send(()).unwrap();
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
            session: Some(
                SessionFacts::verify(input.requested().clone(), ObservedFacts::default()).unwrap(),
            ),
            pause: Some(PauseObservation {
                request: None,
                paused: true,
            }),
            stream_ended: None,
            failure: None,
            audio: AudioAvailability::Disabled,
            readiness: None,
            load_complete: false,
            playback: InitialPlayback::Paused,
            audio_detached: None,
            audio_epoch: None,
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
}
