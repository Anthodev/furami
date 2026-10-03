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
use crate::capture::input::{CaptureSelection, InputSpec, SelectionError};
use crate::domain::capture::{AudioError, AudioSelection, PlaybackGain};

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub video: CaptureSelection,
    pub audio: AudioSelection,
    pub gain: PlaybackGain,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioStatus {
    Disabled,
    Opening,
    Active,
    RestartRequired(AudioError),
}
impl AudioStatus {
    pub fn summary(&self) -> String {
        match self {
            Self::Disabled => "Disabled".into(),
            Self::Opening => "Opening selected audio source".into(),
            Self::Active => "Selected audio track active".into(),
            Self::RestartRequired(diagnostic) => format!("Capture restart required: {diagnostic}"),
        }
    }
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackIntent {
    TogglePause,
    SetGain(PlaybackGain),
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

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub generation: Generation,
    pub initialized: bool,
    pub file_loaded: bool,
    pub playback_started: bool,
    pub session: Option<SessionFacts>,
    pub paused: bool,
    pub ended: bool,
    pub failure: Option<MediaError>,
    pub audio: AudioStatus,
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackendCommand {
    LoadInput,
    TogglePause,
    SetGain(PlaybackGain),
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
    PauseObserved(bool),
    Observed {
        started: bool,
        facts: ObservedFacts,
    },
    AudioStatus(AudioStatus),
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
    fn audio_status(&self) -> AudioStatus {
        AudioStatus::Disabled
    }
    fn shutdown(&mut self) -> Result<(), MediaError>;
}

/// The endpoint contains Rust values only. Dropping it requests stop, never joins.
pub(crate) struct OwnerEndpoint {
    generation: Generation,
    surface: SyncSender<SurfaceToken>,
    commands: SyncSender<PlaybackIntent>,
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
    pub(crate) fn generation(&self) -> Generation {
        self.generation
    }

    pub(crate) fn spawn(
        generation: Generation,
        prefix: String,
        config: SessionConfig,
    ) -> Result<Self, MediaError> {
        let requested = config.video.requested();
        Self::spawn_capture_with_backend(
            generation,
            requested,
            move || config.video.revalidate(),
            move |input| super::ffi::MpvBackend::new(prefix, input, config.audio, config.gain),
        )
    }

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
            move |surface, commands, stop, latest, snapshot| {
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
            move |surface, commands, stop, latest, snapshot| {
                run_backend(
                    factory(),
                    surface,
                    commands,
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
                    paused: false,
                    ended: false,
                    failure: None,
                    audio: AudioStatus::Disabled,
                };
                let outcome = task(
                    &surface_rx,
                    &command_rx,
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
        match self.commands.try_send(intent) {
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

    pub(crate) fn set_gain(&self, generation: Generation, gain: PlaybackGain) -> SubmitStatus {
        self.submit(generation, PlaybackIntent::SetGain(gain))
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
    snapshot.audio = backend.audio_status();
    publish(latest, snapshot);
    let id = RequestId::next(&mut last_request)?;
    backend.submit(id, BackendCommand::LoadInput)?;
    let mut pending = Some((id, BackendCommand::LoadInput));
    loop {
        if stop.is_set() {
            return stop.outcome();
        }
        if pending.is_none() {
            match commands.try_recv() {
                Ok(PlaybackIntent::TogglePause) => {
                    if stop.is_set() {
                        return stop.outcome();
                    }
                    let id = RequestId::next(&mut last_request)?;
                    backend.submit(id, BackendCommand::TogglePause)?;
                    pending = Some((id, BackendCommand::TogglePause));
                }
                Err(TryRecvError::Empty) => {}
                Ok(PlaybackIntent::SetGain(gain)) => {
                    let id = RequestId::next(&mut last_request)?;
                    backend.submit(id, BackendCommand::SetGain(gain))?;
                    pending = Some((id, BackendCommand::SetGain(gain)));
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
            match event {
                BackendEvent::None => {
                    drained = true;
                    break;
                }
                BackendEvent::Other => {}
                BackendEvent::FileLoaded => {
                    if let Some(paused) = backend.refresh_pause()? {
                        snapshot.paused = paused;
                    }
                    snapshot.file_loaded = true;
                    publish(latest, snapshot);
                }
                BackendEvent::PlaybackRestart | BackendEvent::VideoReconfig => {
                    let started = matches!(event, BackendEvent::PlaybackRestart);
                    if let Some(observed) = backend.refresh_observed(started)? {
                        apply_observed(snapshot, requested, started, observed)?;
                        if let Some(paused) = backend.refresh_pause()? {
                            snapshot.paused = paused;
                        }
                        publish(latest, snapshot);
                    }
                }
                BackendEvent::Observed { started, facts } => {
                    apply_observed(snapshot, requested, started, facts)?;
                    publish(latest, snapshot);
                }
                BackendEvent::PauseObserved(paused) => {
                    snapshot.paused = paused;
                    publish(latest, snapshot);
                }
                BackendEvent::AudioStatus(status) => {
                    // Source loss remains latched until explicit whole-session restart.
                    if !matches!(snapshot.audio, AudioStatus::RestartRequired(_)) {
                        snapshot.audio = status;
                        publish(latest, snapshot);
                    }
                }
                BackendEvent::CommandReply { id, error } => {
                    if let Some((expected, kind)) = pending
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
                        pending = None;
                        if kind == BackendCommand::TogglePause {
                            if let Some(paused) = backend.refresh_pause()? {
                                snapshot.paused = paused;
                            }
                            tracing::info!(
                                generation = snapshot.generation.get(),
                                paused = snapshot.paused,
                                "mpv_pause_observed"
                            );
                            publish(latest, snapshot);
                        }
                        // Submit the next FIFO intent before waiting for further events.
                        break;
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
                    if error < 0 || reason == 4 {
                        return Err(MediaError::new(
                            "playback",
                            format!(
                                "mpv END_FILE reason={reason} error={error}; {}",
                                backend.diagnostic().unwrap_or_default()
                            ),
                        ));
                    }
                    if reason == 0 {
                        snapshot.ended = true;
                        publish(latest, snapshot);
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
        if drained {
            thread::park_timeout(Duration::from_millis(10));
        }
    }
}

fn apply_observed(
    snapshot: &mut Snapshot,
    requested: Option<&RequestedFacts>,
    started: bool,
    observed: ObservedFacts,
) -> Result<(), MediaError> {
    if started || snapshot.playback_started {
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
                    if matches!(event, BackendEvent::CommandReply { error: 0, .. }) {
                        self.paused = !self.paused;
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
    fn bounded_commands_do_not_block_priority_stop_or_accept_a_sixty_fifth_slot() {
        let (mut owner, driver) = start(Config::default());
        load(&driver);
        for _ in 0..64 {
            assert_eq!(
                owner.submit(generation(), PlaybackIntent::TogglePause),
                SubmitStatus::Accepted
            );
        }
        assert_eq!(
            owner.submit(generation(), PlaybackIntent::TogglePause),
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
        owner.submit(generation(), PlaybackIntent::TogglePause);
        owner.submit(generation(), PlaybackIntent::TogglePause);
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
        assert_eq!(kind, BackendCommand::TogglePause);
        assert!(first.get() > initial.get());
        assert!(driver.submitted.try_recv().is_err());
        driver.send(BackendEvent::CommandReply {
            id: first.get(),
            error: 0,
        });
        let (second, kind) = driver.submitted.recv().unwrap();
        assert_eq!(kind, BackendCommand::TogglePause);
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
        owner.submit(generation(), PlaybackIntent::TogglePause);
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
                owner.submit(generation(), PlaybackIntent::TogglePause);
                driver.send(BackendEvent::CommandReply {
                    id: initial.get(),
                    error: 0,
                });
                assert_eq!(
                    driver.submitted.recv().unwrap().1,
                    BackendCommand::TogglePause
                );
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
                owner.submit(generation(), PlaybackIntent::TogglePause);
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
        assert!(snapshot.file_loaded && snapshot.ended);
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
            owner.submit(stale, PlaybackIntent::TogglePause),
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
            owner.set_gain(Generation::new(2).unwrap(), PlaybackGain::default()),
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
    fn source_failure_stays_explicit_despite_late_active_then_native_failure_is_terminal() {
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
        driver.send(BackendEvent::AudioStatus(AudioStatus::RestartRequired(
            failure.clone(),
        )));
        driver.send(BackendEvent::AudioStatus(AudioStatus::Active));
        driver.send(BackendEvent::FileLoaded);
        driver.fence();
        let snapshot = owner.take_snapshot().unwrap();
        assert!(snapshot.file_loaded);
        assert_eq!(snapshot.audio, AudioStatus::RestartRequired(failure));
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
