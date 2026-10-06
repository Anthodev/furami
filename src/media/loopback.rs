//! One owned host pw-loopback child and public Pulse controls. No PCM processing.

use super::controller::Generation;
use crate::{
    capture::{
        audio::NativeSourceSnapshot,
        linux::pulse::{LoopStream, LoopbackPulse},
    },
    domain::{
        capture::{AudioEpoch, AudioError, AudioSourceIdentity, PlaybackGain, WatchStamp},
        output::{LiveSinkTarget, OutputPlan, OutputRevision, OutputSilence},
        state::AttemptId,
    },
};
use parking_lot::Mutex;
use std::{
    fs::File,
    io::Read,
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const HELPER: &str = "/usr/bin/pw-loopback";
const METADATA_HELPER: &str = "/usr/bin/pw-metadata";
const MAX_METADATA_STDERR: usize = 4096;
const INVALID_INDEX: u32 = u32::MAX;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const TERM_TIMEOUT: Duration = Duration::from_secs(2);
const KILL_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);
const REMOVED_ENV: &[&str] = &[
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "PIPEWIRE_MODULE_DIR",
    "SPA_PLUGIN_DIR",
    "SPA_SUPPORT_LIB",
    "PIPEWIRE_CONFIG_DIR",
    "PIPEWIRE_CONFIG_PREFIX",
    "PIPEWIRE_CONFIG_NAME",
    "PIPEWIRE_PROPS",
    "PIPEWIRE_NODE",
    "PIPEWIRE_AUTOCONNECT",
    "PIPEWIRE_QUANTUM",
    "PIPEWIRE_LATENCY",
    "PIPEWIRE_RATE",
    "PIPEWIRE_DAEMON",
    "PIPEWIRE_NO_CONFIG",
];

/// Correlation only. A source serial is obtained by the subscribed worker, not
/// supplied by a caller or restored from preferences.
#[derive(Clone, Debug)]
pub struct LoopbackOpen {
    pub generation: Generation,
    pub attempt: AttemptId,
    pub epoch: AudioEpoch,
    pub source: AudioSourceIdentity,
    pub watch: WatchStamp,
    pub output: LiveSinkTarget,
    pub output_revision: OutputRevision,
    pub gain: PlaybackGain,
}

#[derive(Clone, Debug)]
pub enum LoopbackCommand {
    SetGain(PlaybackGain),
    SetOutput(OutputPlan),
    /// Pause retires/reaps capture; it never freezes and later unpauses old PCM.
    Pause,
    Retire,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct OwnedStreamReceipt {
    index: u32,
    serial: u64,
    global_id: Option<u32>,
    node: String,
}
impl OwnedStreamReceipt {
    pub fn index(&self) -> u32 {
        self.index
    }
    pub fn serial(&self) -> u64 {
        self.serial
    }
    pub fn global_id(&self) -> Option<u32> {
        self.global_id
    }
    pub fn node(&self) -> &str {
        &self.node
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopbackReceipt {
    inner: Arc<LoopbackReceiptData>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct LoopbackReceiptData {
    generation: Generation,
    attempt: AttemptId,
    epoch: AudioEpoch,
    watch: WatchStamp,
    source: AudioSourceIdentity,
    source_serial: u64,
    source_index: u32,
    child_pid: u32,
    capture: OwnedStreamReceipt,
    playback: OwnedStreamReceipt,
    destination: Option<LiveSinkTarget>,
    output_revision: OutputRevision,
    gain: PlaybackGain,
    effective_muted: bool,
}
impl serde::Serialize for LoopbackReceipt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(self.inner.as_ref(), serializer)
    }
}
impl LoopbackReceipt {
    pub fn generation(&self) -> Generation {
        self.inner.generation
    }
    pub fn attempt(&self) -> AttemptId {
        self.inner.attempt
    }
    pub fn epoch(&self) -> AudioEpoch {
        self.inner.epoch
    }
    pub fn watch(&self) -> WatchStamp {
        self.inner.watch
    }
    pub fn source(&self) -> &AudioSourceIdentity {
        &self.inner.source
    }
    pub fn source_serial(&self) -> u64 {
        self.inner.source_serial
    }
    pub fn source_index(&self) -> u32 {
        self.inner.source_index
    }
    pub fn child_pid(&self) -> u32 {
        self.inner.child_pid
    }
    pub fn capture(&self) -> &OwnedStreamReceipt {
        &self.inner.capture
    }
    pub fn playback(&self) -> &OwnedStreamReceipt {
        &self.inner.playback
    }
    pub fn destination(&self) -> Option<&LiveSinkTarget> {
        self.inner.destination.as_ref()
    }
    pub fn output_revision(&self) -> OutputRevision {
        self.inner.output_revision
    }
    pub fn gain(&self) -> PlaybackGain {
        self.inner.gain
    }
    pub fn effective_muted(&self) -> bool {
        self.inner.effective_muted
    }

    /// A value-only boundary fixture; never a production admission path.
    #[cfg(test)]
    pub(crate) fn for_test(
        generation: Generation,
        attempt: AttemptId,
        epoch: AudioEpoch,
        watch: WatchStamp,
        source: AudioSourceIdentity,
        destination: LiveSinkTarget,
        output_revision: OutputRevision,
    ) -> Self {
        Self {
            inner: Arc::new(LoopbackReceiptData {
                generation,
                attempt,
                epoch,
                watch,
                source,
                source_serial: 11,
                source_index: 7,
                child_pid: 42,
                capture: OwnedStreamReceipt {
                    index: 8,
                    serial: 12,
                    global_id: Some(18),
                    node: "fixture.capture".into(),
                },
                playback: OwnedStreamReceipt {
                    index: 9,
                    serial: 13,
                    global_id: Some(19),
                    node: "fixture.playback".into(),
                },
                destination: Some(destination),
                output_revision,
                gain: PlaybackGain::default(),
                effective_muted: false,
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub enum LoopbackEvent {
    Opening,
    /// Genuine observed unlinked playback, mute and gain already read back.
    UnlinkedReady(LoopbackReceipt),
    Active(LoopbackReceipt),
    Switching {
        revision: OutputRevision,
    },
    Silent {
        reason: OutputSilence,
        receipt: LoopbackReceipt,
    },
    SourceLost(AudioError),
    Failed(AudioError),
    Blocked {
        stage: &'static str,
        diagnostic: String,
    },
    /// Created by the supervisor only after worker join and real child reap.
    Stopped {
        outcome: Result<(), AudioError>,
    },
}

struct Seed {
    generation: Generation,
    attempt: AttemptId,
    epoch: AudioEpoch,
    source: AudioSourceIdentity,
    watch: WatchStamp,
    output: LiveSinkTarget,
    output_revision: OutputRevision,
    gain: PlaybackGain,
}
impl From<LoopbackOpen> for Seed {
    fn from(open: LoopbackOpen) -> Self {
        Self {
            generation: open.generation,
            attempt: open.attempt,
            epoch: open.epoch,
            source: open.source,
            watch: open.watch,
            output: open.output,
            output_revision: open.output_revision,
            gain: open.gain,
        }
    }
}

#[derive(Clone, Debug)]
struct Desired {
    gain: PlaybackGain,
    output: OutputPlan,
    serial: u64,
}
struct Shared {
    stop: Arc<AtomicBool>,
    owner_cancel: Arc<AtomicBool>,
    paused: AtomicBool,
    desired: Mutex<Desired>,
    event: Mutex<Option<LoopbackEvent>>,
}
impl Shared {
    fn cancelled(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.owner_cancel.load(Ordering::Acquire)
    }
    fn publish(&self, event: LoopbackEvent) {
        *self.event.lock() = Some(event);
    }
    fn current(&self, desired: &Desired) -> bool {
        !self.cancelled() && self.desired.lock().serial == desired.serial
    }
    fn admission(&self, expected: &Desired) -> Option<parking_lot::MutexGuard<'_, Desired>> {
        let current = self.desired.lock();
        (!self.cancelled() && current.serial == expected.serial).then_some(current)
    }
}

/// Commands/poll/join are nonblocking. Drop is a last-resort real lifetime
/// barrier; the normal aggregate must stop + try_join before dropping this owner.
#[doc(hidden)]
pub struct LoopbackOwner {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<Result<(), AudioError>>>,
    joined: bool,
}
impl LoopbackOwner {
    pub fn begin(open: LoopbackOpen, owner_cancel: Arc<AtomicBool>) -> Result<Self, AudioError> {
        Self::start(open.into(), owner_cancel)
    }

    fn start(open: Seed, owner_cancel: Arc<AtomicBool>) -> Result<Self, AudioError> {
        PlaybackGain::new(open.gain.volume_percent, open.gain.muted)?;
        if owner_cancel.load(Ordering::Acquire) {
            return Err(AudioError::Cancelled);
        }
        let names = Names::new(open.generation, open.epoch)?;
        let shared = Arc::new(Shared {
            stop: Arc::new(AtomicBool::new(false)),
            owner_cancel,
            paused: AtomicBool::new(false),
            desired: Mutex::new(Desired {
                gain: open.gain,
                output: OutputPlan::Target {
                    revision: open.output_revision,
                    target: open.output.clone(),
                },
                serial: 1,
            }),
            event: Mutex::new(Some(LoopbackEvent::Opening)),
        });
        let state = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name(format!("audio-loopback-{}", open.generation.get()))
            .spawn(move || {
                let mut outcome = run(open, names, &state);
                if outcome == Err(AudioError::Cancelled) && state.cancelled() {
                    outcome = Ok(());
                }
                if let Err(error) = &outcome {
                    state.publish(
                        if matches!(
                            error,
                            AudioError::SourceMissing { .. } | AudioError::SourceChanged { .. }
                        ) {
                            LoopbackEvent::SourceLost(error.clone())
                        } else {
                            LoopbackEvent::Failed(error.clone())
                        },
                    );
                }
                outcome
            })
            .map_err(|error| AudioError::Unavailable(format!("loopback worker: {error}")))?;
        Ok(Self {
            shared,
            worker: Some(worker),
            joined: false,
        })
    }

    pub fn submit(&self, command: LoopbackCommand) -> Result<(), AudioError> {
        if matches!(command, LoopbackCommand::Retire | LoopbackCommand::Pause) {
            if matches!(command, LoopbackCommand::Pause) {
                self.shared.paused.store(true, Ordering::Release);
            }
            self.stop();
            return Ok(());
        }
        if self.shared.cancelled() {
            return Err(AudioError::Cancelled);
        }
        let mut desired = self.shared.desired.lock();
        if self.shared.cancelled() {
            return Err(AudioError::Cancelled);
        }
        let next_serial = desired
            .serial
            .checked_add(1)
            .ok_or_else(|| AudioError::Control("loopback intent revision exhausted".into()))?;
        match command {
            LoopbackCommand::SetGain(gain) => {
                PlaybackGain::new(gain.volume_percent, gain.muted)?;
                if desired.gain == gain {
                    return Ok(());
                }
                desired.gain = gain;
            }
            LoopbackCommand::SetOutput(output) => {
                let old_revision = plan_revision(&desired.output);
                let new_revision = plan_revision(&output);
                if new_revision.get() < old_revision.get() {
                    return Err(AudioError::Control("stale output revision".into()));
                }
                if new_revision == old_revision {
                    if desired.output == output {
                        return Ok(());
                    }
                    return Err(AudioError::Control(
                        "different output plans share a revision".into(),
                    ));
                }
                desired.output = output;
            }
            LoopbackCommand::Pause | LoopbackCommand::Retire => {
                unreachable!("handled outside mailbox")
            }
        }
        desired.serial = next_serial;
        Ok(())
    }

    pub fn poll(&mut self) -> Option<LoopbackEvent> {
        if let Some(event) = self.shared.event.lock().take() {
            return Some(event);
        }
        self.try_join()
            .map(|outcome| LoopbackEvent::Stopped { outcome })
    }
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }
    pub fn try_join(&mut self) -> Option<Result<(), AudioError>> {
        if self.joined || !self.worker.as_ref()?.is_finished() {
            return None;
        }
        self.joined = true;
        Some(
            self.worker
                .take()
                .expect("checked worker")
                .join()
                .map_err(|_| {
                    AudioError::Control("loopback worker panicked; no clean retirement".into())
                })
                .and_then(|outcome| outcome),
        )
    }

    /// Native-free control/lifetime boundary fixture: a real cancelling owner
    /// worker remains joinable only after the test releases its barrier.
    #[cfg(test)]
    pub(crate) fn for_test_retiring_worker(
        outcome: Result<(), AudioError>,
    ) -> (Self, std::sync::mpsc::Sender<()>) {
        let shared = Arc::new(Shared {
            stop: Arc::new(AtomicBool::new(true)),
            owner_cancel: Arc::new(AtomicBool::new(false)),
            paused: AtomicBool::new(false),
            desired: Mutex::new(Desired {
                gain: PlaybackGain::default(),
                output: OutputPlan::Silent {
                    revision: OutputRevision::first(),
                    reason: OutputSilence::ManualRequiresAction,
                },
                serial: 1,
            }),
            event: Mutex::new(None),
        });
        let (release, barrier) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            barrier
                .recv()
                .map_err(|_| AudioError::Control("test retirement barrier closed".into()))?;
            outcome
        });
        (
            Self {
                shared,
                worker: Some(worker),
                joined: false,
            },
            release,
        )
    }
}
impl Drop for LoopbackOwner {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Debug)]
struct Names {
    app: String,
    capture: String,
    playback: String,
    parking: String,
}
impl Names {
    fn new(generation: Generation, epoch: AudioEpoch) -> Result<Self, AudioError> {
        let mut nonce = [0u8; 16];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut nonce))
            .map_err(|error| AudioError::Unavailable(format!("loopback tag entropy: {error}")))?;
        let app = format!(
            "furami.{}.{}.{}.{:032x}",
            std::process::id(),
            generation.get(),
            epoch.get(),
            u128::from_ne_bytes(nonce)
        );
        Ok(Self {
            capture: format!("{app}.capture"),
            playback: format!("{app}.playback"),
            parking: format!("{app}.unlinked"),
            app,
        })
    }
}

fn plan_revision(plan: &OutputPlan) -> OutputRevision {
    match plan {
        OutputPlan::Target { revision, .. } | OutputPlan::Silent { revision, .. } => *revision,
    }
}
fn native_volume(gain: PlaybackGain) -> u32 {
    // PA volume is cubic-coded: 50% PA native volume decodes to .5^3.
    // The current UI response is preserved without cubing a second time.
    (u32::from(gain.volume_percent) * 0x10000 + 50) / 100
}
fn effective_mute(user: bool, output_latch: bool, switching: bool, paused: bool) -> bool {
    user || output_latch || switching || paused
}

fn properties(entries: &[(&str, String)]) -> Result<String, AudioError> {
    // PipeWire's checked property parser accepts JSON objects. serde_json
    // handles quotes, backslashes, control characters and Unicode correctly.
    let mut object = serde_json::Map::new();
    for (key, value) in entries {
        object.insert((*key).into(), serde_json::Value::String(value.clone()));
    }
    serde_json::to_string(&object)
        .map_err(|error| AudioError::InvalidSelection(format!("loopback properties: {error}")))
}
fn child_command(helper: &Path, names: &Names, source_serial: u64) -> Result<Command, AudioError> {
    let capture = properties(&[
        ("application.name", names.app.clone()),
        ("media.name", names.capture.clone()),
        ("target.object", source_serial.to_string()),
        ("node.dont-fallback", "true".into()),
        ("node.dont-reconnect", "true".into()),
        ("node.dont-move", "true".into()),
        ("state.restore-target", "false".into()),
    ])?;
    let playback = properties(&[
        ("application.name", names.app.clone()),
        ("media.name", names.playback.clone()),
        ("target.object", names.parking.clone()),
        ("node.dont-fallback", "true".into()),
        ("node.linger", "true".into()),
        ("node.dont-reconnect", "false".into()),
        ("node.dont-move", "false".into()),
        ("state.restore-target", "false".into()),
        ("state.restore-props", "false".into()),
    ])?;
    let mut command = Command::new(helper);
    // pw-loopback 1.6.9 derives its default name from argv[0] + getpid();
    // module-loopback prefixes input./output. Never override these node names:
    // the observed names then encode the real helper PID, without a shell,
    // launch shim, pre_exec allocation or a fake application.process.id.
    command.arg0("furami-loopback");
    command.args(["--capture-props", &capture, "--playback-props", &playback]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    for key in REMOVED_ENV {
        command.env_remove(key);
    }
    Ok(command)
}

struct OwnedChild {
    child: Child,
    reaped: bool,
}
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetirementStage {
    Term,
    Kill,
    Blocked,
    Reaped,
}
fn retirement_stage(elapsed: Duration, reaped: bool) -> RetirementStage {
    if reaped {
        RetirementStage::Reaped
    } else if elapsed < TERM_TIMEOUT {
        RetirementStage::Term
    } else if elapsed < TERM_TIMEOUT + KILL_TIMEOUT {
        RetirementStage::Kill
    } else {
        RetirementStage::Blocked
    }
}

fn retain_blocked(shared: &Shared, stage: &'static str, diagnostic: String) -> ! {
    shared.publish(LoopbackEvent::Blocked { stage, diagnostic });
    loop {
        thread::park();
    }
}

impl OwnedChild {
    fn spawn(command: &mut Command) -> Result<Self, AudioError> {
        let child = command.spawn().map_err(|error| AudioError::Backend {
            operation: format!("spawn {}", command.get_program().to_string_lossy()),
            code: error.raw_os_error(),
            detail: error.to_string(),
        })?;
        Ok(Self {
            child,
            reaped: false,
        })
    }
    fn pid(&self) -> u32 {
        self.child.id()
    }
    fn alive(&mut self) -> Result<(), AudioError> {
        match self.child.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(status)) => {
                self.reaped = true;
                Err(AudioError::RecordingLost {
                    detail: format!("owned pw-loopback exited: {status}"),
                })
            }
            Err(error) => Err(AudioError::Control(format!("owned child status: {error}"))),
        }
    }
    fn observed_exit(&mut self, shared: &Shared, stage: &'static str) -> bool {
        match self.child.try_wait() {
            Ok(None) => false,
            Ok(Some(_)) => {
                self.reaped = true;
                true
            }
            Err(error) => retain_blocked(
                shared,
                stage,
                format!("owned child reap status failed: {error}; no Stopped proof"),
            ),
        }
    }
    fn retire(&mut self, shared: &Shared) -> Result<(), AudioError> {
        if self.reaped {
            return Ok(());
        }
        if self.observed_exit(shared, "child-pre-signal") {
            return Ok(());
        }
        let pid = i32::try_from(self.pid())
            .map_err(|_| AudioError::Control("owned PID outside Linux pid_t".into()))?;
        // SAFETY: This exact unreaped Child retains its PID, so it cannot be
        // recycled into a foreign process between try_wait and signal delivery.
        let signal_error = (unsafe { kill(pid, 15) } != 0).then(std::io::Error::last_os_error);
        let started = Instant::now();
        while retirement_stage(started.elapsed(), false) == RetirementStage::Term {
            if self.observed_exit(shared, "child-TERM-wait") {
                self.reaped = true;
                return match signal_error {
                    Some(error) if error.raw_os_error() != Some(3) => {
                        Err(AudioError::Control(format!("child TERM: {error}")))
                    }
                    _ => Ok(()),
                };
            }
            thread::sleep(POLL);
        }
        let kill_error = self.child.kill().err();
        while retirement_stage(started.elapsed(), false) == RetirementStage::Kill {
            if self.observed_exit(shared, "child-KILL-wait") {
                self.reaped = true;
                return match kill_error {
                    Some(error) if error.raw_os_error() != Some(3) => {
                        Err(AudioError::Control(format!("child KILL: {error}")))
                    }
                    _ => Ok(()),
                };
            }
            thread::sleep(POLL);
        }
        shared.publish(LoopbackEvent::Blocked { stage: "child-reap", diagnostic: format!("owned child {} remains unreaped after TERM/KILL deadlines; kill error {kill_error:?}", self.pid()) });
        // No timeout manufactures Stopped. The worker retains ownership and
        // waits for actual kernel reaping; the supervisor remains responsive.
        if let Err(error) = self.child.wait() {
            retain_blocked(
                shared,
                "child-blocking-reap",
                format!("owned child wait failed: {error}; no Stopped proof"),
            );
        }
        self.reaped = true;
        Err(AudioError::Control(
            "owned child exceeded bounded retirement".into(),
        ))
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Panic/error paths still cannot detach an owned helper. Never signal by
        // process name or global enumeration. Unreaped lifetime protects PID.
        if !self.reaped {
            let _ = self.child.kill();
            match self.child.wait() {
                Ok(_) => self.reaped = true,
                Err(_) => loop {
                    thread::park();
                },
            }
        }
    }
}

fn related(row: &LoopStream, app: &str, node: &str) -> bool {
    row.app.as_deref() == Some(app)
        || row.node.as_deref() == Some(node)
        || row.media.as_deref() == Some(node)
}
fn generated_node(node: Option<&str>, capture: bool, pid: u32) -> bool {
    let prefix = if capture {
        "input.furami-loopback-"
    } else {
        "output.furami-loopback-"
    };
    node.and_then(|name| name.strip_prefix(prefix))
        .is_some_and(|suffix| {
            !suffix.is_empty()
                && !suffix.starts_with('0')
                && suffix.bytes().all(|byte| byte.is_ascii_digit())
                && suffix.parse::<u32>().ok() == Some(pid)
        })
}

fn attributed(
    rows: Vec<LoopStream>,
    names: &Names,
    capture: bool,
    pid: u32,
) -> Result<Option<LoopStream>, AudioError> {
    let media = if capture {
        &names.capture
    } else {
        &names.playback
    };
    let mut matches = rows.into_iter().filter(|row| {
        related(row, &names.app, media) || generated_node(row.node.as_deref(), capture, pid)
    });
    let Some(row) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(AudioError::Ambiguous(
            "multiple streams claim this owned opening".into(),
        ));
    }
    if row.index == INVALID_INDEX
        || row.serial.is_none()
        || row.app.as_deref() != Some(names.app.as_str())
        || !generated_node(row.node.as_deref(), capture, pid)
        || row.media.as_deref() != Some(media.as_str())
        || row
            .pid
            .as_deref()
            .is_some_and(|property| property.parse::<u32>().ok() != Some(pid))
    {
        return Err(AudioError::Control(
            "stream claimed tag without exact generated child-PID node/media/application/serial"
                .into(),
        ));
    }
    Ok(Some(row))
}
fn stream_receipt(row: &LoopStream) -> OwnedStreamReceipt {
    OwnedStreamReceipt {
        index: row.index,
        serial: row.serial.expect("attributed serial"),
        global_id: row.global_id,
        node: row.node.clone().expect("attributed node"),
    }
}
fn same_stream(expected: &OwnedStreamReceipt, row: &LoopStream) -> bool {
    expected.index == row.index
        && Some(expected.serial) == row.serial
        && row.node.as_deref() == Some(expected.node.as_str())
        && expected
            .global_id
            .is_none_or(|id| row.global_id == Some(id))
}
fn same_source(
    expected: &NativeSourceSnapshot,
    current: &NativeSourceSnapshot,
) -> Result<(), AudioError> {
    if expected.serial != current.serial
        || expected.pulse_index != current.pulse_index
        || expected.identity != current.identity
    {
        return Err(AudioError::SourceChanged {
            name: expected.identity.name().into(),
        });
    }
    Ok(())
}
fn fresh_stream(
    pulse: &mut LoopbackPulse,
    names: &Names,
    child: &mut OwnedChild,
    capture: bool,
    expected: &OwnedStreamReceipt,
) -> Result<LoopStream, AudioError> {
    child.alive()?;
    pulse.check_lifetime()?;
    let row = attributed(
        pulse.streams(capture, Some(expected.index))?,
        names,
        capture,
        child.pid(),
    )?
    .ok_or_else(|| AudioError::RecordingLost {
        detail: "owned stream no longer present".into(),
    })?;
    // A row can be copied before a REMOVE is delivered in the same blocking
    // PA dispatch. Recheck sticky lifetime and actual Child AFTER that wait.
    pulse.check_lifetime()?;
    child.alive()?;
    if !capture && row.global_id.is_none() {
        return Err(AudioError::MissingGlobalId {
            node: row.node.clone().unwrap_or_default(),
        });
    }
    if !same_stream(expected, &row) {
        return Err(AudioError::Control("owned stream index replaced".into()));
    }
    Ok(row)
}
fn fresh_target(pulse: &mut LoopbackPulse, target: &LiveSinkTarget) -> Result<bool, AudioError> {
    let sinks = pulse.sinks()?;
    let mut rows = sinks
        .iter()
        .filter(|row| row.target.identity.name() == target.identity.name());
    let Some(row) = rows.next() else {
        return Ok(false);
    };
    if rows.next().is_some() {
        return Err(AudioError::Ambiguous(
            "multiple sinks match selected name".into(),
        ));
    }
    Ok(row.target == *target)
}
fn receipt(
    open: &LoopbackOpen,
    source: &NativeSourceSnapshot,
    child: &OwnedChild,
    streams: (&OwnedStreamReceipt, &OwnedStreamReceipt),
    destination: Option<LiveSinkTarget>,
    desired: &Desired,
    muted: bool,
) -> LoopbackReceipt {
    let (capture, playback) = streams;
    LoopbackReceipt {
        inner: Arc::new(LoopbackReceiptData {
            generation: open.generation,
            attempt: open.attempt,
            epoch: open.epoch,
            watch: open.watch,
            source: open.source.clone(),
            source_serial: source.serial,
            source_index: source.pulse_index,
            child_pid: child.pid(),
            capture: capture.clone(),
            playback: playback.clone(),
            destination,
            output_revision: plan_revision(&desired.output),
            gain: desired.gain,
            effective_muted: muted,
        }),
    }
}

fn confirm_mute(
    pulse: &mut LoopbackPulse,
    names: &Names,
    child: &mut OwnedChild,
    playback: &OwnedStreamReceipt,
    muted: bool,
) -> Result<LoopStream, AudioError> {
    let row = fresh_stream(pulse, names, child, false, playback)?;
    if row.muted != muted {
        pulse.mute(row.index, muted)?;
    }
    loop {
        let row = fresh_stream(pulse, names, child, false, playback)?;
        if row.muted == muted {
            return Ok(row);
        }
        thread::sleep(POLL);
    }
}

fn confirm_admitted_mute(
    pulse: &mut LoopbackPulse,
    names: &Names,
    child: &mut OwnedChild,
    playback: &OwnedStreamReceipt,
    muted: bool,
    shared: &Shared,
    desired: &Desired,
) -> Result<Option<LoopStream>, AudioError> {
    if muted {
        return confirm_mute(pulse, names, child, playback, true).map(Some);
    }
    let fresh = fresh_stream(pulse, names, child, false, playback)?;
    // This lookup may have allowed a newer mute/silent intent to be accepted.
    // Admission is therefore checked while retaining the intent lock through
    // the ACTUAL enqueue below, not merely before the lookup or after its ACK.
    if !pulse.mute_admitted(fresh.index, false, || shared.admission(desired))? {
        return Ok(None);
    }
    loop {
        if !shared.current(desired) {
            confirm_mute(pulse, names, child, playback, true)?;
            return Ok(None);
        }
        let row = fresh_stream(pulse, names, child, false, playback)?;
        if !row.muted {
            return Ok(Some(row));
        }
        thread::sleep(POLL);
    }
}

fn output_latched(desired: OutputRevision, removed_applied: Option<OutputRevision>) -> bool {
    removed_applied.is_some_and(|removed| desired.get() <= removed.get())
}

fn confirm_gain(
    pulse: &mut LoopbackPulse,
    names: &Names,
    child: &mut OwnedChild,
    playback: &OwnedStreamReceipt,
    gain: PlaybackGain,
) -> Result<(), AudioError> {
    let row = fresh_stream(pulse, names, child, false, playback)?;
    if !row.volume_writable || row.volumes.is_empty() {
        return Err(AudioError::Control(
            "owned playback has no writable volume".into(),
        ));
    }
    let volume = native_volume(gain);
    if row.volumes.iter().any(|current| *current != volume) {
        pulse.volume(row.index, row.volumes.len(), volume)?;
    }
    loop {
        let row = fresh_stream(pulse, names, child, false, playback)?;
        if !row.volume_writable || row.volumes.is_empty() {
            return Err(AudioError::Control(
                "owned playback volume support disappeared during readback".into(),
            ));
        }
        // Native float/PA conversion can differ by one encoded unit.
        if row
            .volumes
            .iter()
            .all(|current| current.abs_diff(volume) <= 1)
        {
            return Ok(());
        }
        thread::sleep(POLL);
    }
}
fn check_capture(
    pulse: &mut LoopbackPulse,
    names: &Names,
    child: &mut OwnedChild,
    source: &NativeSourceSnapshot,
    capture: &OwnedStreamReceipt,
) -> Result<(), AudioError> {
    same_source(source, &pulse.source(&source.identity)?)?;
    let current = fresh_stream(pulse, names, child, true, capture)?;
    check_capture_target(source, &current)
}

pub(crate) fn check_capture_target(
    source: &NativeSourceSnapshot,
    current: &LoopStream,
) -> Result<(), AudioError> {
    if current.target != source.pulse_index {
        return Err(AudioError::SourceChanged {
            name: source.identity.name().into(),
        });
    }
    Ok(())
}

fn metadata_subject(row: &LoopStream) -> Result<u32, AudioError> {
    row.global_id
        .filter(|id| *id != 0 && *id != INVALID_INDEX)
        .ok_or_else(|| AudioError::MissingGlobalId {
            node: row.node.clone().unwrap_or_default(),
        })
}

fn metadata_command(subject: u32, sink_serial: u64) -> Result<(Command, UnixStream), AudioError> {
    // object.id is freshly observed on OUR native playback node. Never convert
    // a Pulse index/serial into a global ID, and never write core subject zero.
    if subject == 0 || subject == INVALID_INDEX {
        return Err(AudioError::MissingGlobalId {
            node: "metadata subject".into(),
        });
    }
    let (reader, writer) = UnixStream::pair()
        .map_err(|error| AudioError::Control(format!("metadata stderr channel: {error}")))?;
    reader
        .set_nonblocking(true)
        .map_err(|error| AudioError::Control(format!("metadata stderr nonblocking: {error}")))?;
    let mut command = Command::new(METADATA_HELPER);
    // Pinned CLI positional order is id, key, VALUE, TYPE; no --monitor.
    command.args([
        "-n",
        "default",
        &subject.to_string(),
        "target.object",
        &sink_serial.to_string(),
        "Spa:Id",
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(OwnedFd::from(writer)));
    for key in REMOVED_ENV {
        command.env_remove(key);
    }
    Ok((command, reader))
}

fn drain_metadata_stderr(
    reader: &mut UnixStream,
    bytes: &mut Vec<u8>,
    truncated: &mut bool,
) -> Result<(), AudioError> {
    let mut buffer = [0u8; 1024];
    // Bound both retained bytes and work per iteration, even for a noisy tool.
    let mut drained = 0;
    while drained < 8192 {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let keep = count.min(MAX_METADATA_STDERR - bytes.len());
                bytes.extend_from_slice(&buffer[..keep]);
                *truncated |= keep != count;
                drained += count;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                break;
            }
            Err(error) => {
                return Err(AudioError::Control(format!(
                    "metadata stderr read: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn write_owned_target(
    pulse: &mut LoopbackPulse,
    names: &Names,
    audio_child: &mut OwnedChild,
    source: &NativeSourceSnapshot,
    streams: (&OwnedStreamReceipt, &OwnedStreamReceipt),
    target: &LiveSinkTarget,
    operation: (&Shared, &Desired, Instant),
) -> Result<bool, AudioError> {
    let (shared, desired, deadline) = operation;
    let (capture, playback) = streams;
    // Revalidation observes the exact row supplying the subject, not an old
    // numeric cache or a merely similar node. The capture child stays live.
    check_capture(pulse, names, audio_child, source, capture)?;
    let row = fresh_stream(pulse, names, audio_child, false, playback)?;
    let subject = metadata_subject(&row)?;
    if !fresh_target(pulse, target)? {
        return Ok(false);
    }
    let (mut command, mut stderr) = metadata_command(subject, target.object_serial.get())?;
    check_capture(pulse, names, audio_child, source, capture)?;
    let before_launch = fresh_stream(pulse, names, audio_child, false, playback)?;
    if metadata_subject(&before_launch)? != subject {
        return Err(AudioError::Control(
            "owned playback subject changed before metadata launch".into(),
        ));
    }
    if Instant::now() >= deadline {
        return Err(AudioError::Backend {
            operation: "owned pw-metadata pre-launch deadline".into(),
            code: None,
            detail: "ownership validation exceeded the operation deadline".into(),
        });
    }
    let Some(guard) = shared.admission(desired) else {
        return Ok(false);
    };
    pulse.check_lifetime()?;
    audio_child.alive()?;
    let spawned = OwnedChild::spawn(&mut command);
    drop(guard); // Never retain Qt's intent mutex through child/PA waits.
    let mut writer = spawned?;
    // Command owns a duplicate of stderr's writer FD; close it after spawn so
    // actual child exit can produce EOF on our nonblocking diagnostics reader.
    drop(command);
    let mut bytes = Vec::with_capacity(MAX_METADATA_STDERR);
    let mut truncated = false;
    let result = (|| {
        loop {
            if !shared.current(desired) {
                return Ok(false);
            }
            if Instant::now() >= deadline {
                return Err(AudioError::Backend {
                    operation: "owned pw-metadata deadline".into(),
                    code: None,
                    detail: format!(
                        "stderr={:?}, truncated={truncated}",
                        String::from_utf8_lossy(&bytes)
                    ),
                });
            }
            check_capture(pulse, names, audio_child, source, capture)?;
            let fresh = fresh_stream(pulse, names, audio_child, false, playback)?;
            if metadata_subject(&fresh)? != subject {
                return Err(AudioError::Control(
                    "owned native playback global ID changed during target write".into(),
                ));
            }
            if !fresh_target(pulse, target)? {
                return Ok(false);
            }
            drain_metadata_stderr(&mut stderr, &mut bytes, &mut truncated)?;
            if let Some(status) = writer
                .child
                .try_wait()
                .map_err(|error| AudioError::Control(format!("metadata child status: {error}")))?
            {
                writer.reaped = true;
                drain_metadata_stderr(&mut stderr, &mut bytes, &mut truncated)?;
                eprintln!(
                    "LOOPBACK_METADATA_CONTROL_NOT_ROUTE_PROOF pid={} subject={subject} target_serial={} status={status} stderr={:?} truncated={truncated}",
                    writer.pid(),
                    target.object_serial,
                    String::from_utf8_lossy(&bytes)
                );
                if !status.success() {
                    return Err(AudioError::Backend {
                        operation: "owned pw-metadata completion".into(),
                        code: status.code(),
                        detail: format!(
                            "status={status}; stderr={:?}; truncated={truncated}",
                            String::from_utf8_lossy(&bytes)
                        ),
                    });
                }
                // Exit zero/core.sync can coexist with asynchronous errors or
                // absent metadata. The caller still awaits an ACTUAL PA peer.
                return Ok(true);
            }
            thread::sleep(POLL);
        }
    })();
    // All cancel/error/timeout paths TERM/KILL/reap this exact short child.
    // Capture retirement cannot acknowledge while either child remains live.
    let cleanup = writer.retire(shared);
    drain_metadata_stderr(&mut stderr, &mut bytes, &mut truncated)?;
    match (result, cleanup) {
        (_, Err(error)) => Err(error),
        (result, Ok(())) => result,
    }
}

fn run(seed: Seed, names: Names, shared: &Shared) -> Result<(), AudioError> {
    if shared.cancelled() {
        return Ok(());
    }
    let mut pulse =
        LoopbackPulse::connect(Arc::clone(&shared.stop), Arc::clone(&shared.owner_cancel))?;
    pulse.deadline(Instant::now() + OPERATION_TIMEOUT);
    let source = pulse.source(&seed.source)?;
    // Close the subscribe/first-scan gap before the helper has any authority.
    same_source(&source, &pulse.source(&seed.source)?)?;
    let watch = seed.watch;
    let open = LoopbackOpen {
        generation: seed.generation,
        attempt: seed.attempt,
        epoch: seed.epoch,
        source: seed.source,
        watch,
        output: seed.output,
        output_revision: seed.output_revision,
        gain: seed.gain,
    };
    if pulse
        .sinks()?
        .iter()
        .any(|row| row.target.identity.name() == names.parking)
    {
        return Err(AudioError::Control(
            "private unlinked target unexpectedly exists".into(),
        ));
    }
    if shared.cancelled() {
        return Ok(());
    }
    let mut child = OwnedChild::spawn(&mut child_command(
        Path::new(HELPER),
        &names,
        source.serial,
    )?)?;
    let mut result = run_child(&open, &names, shared, &mut pulse, &source, &mut child);
    if matches!(&result, Err(AudioError::RecordingLost { .. })) && !shared.cancelled() {
        // Helper teardown can arrive before Pulse's source-removal callback.
        // One fresh read attributes a confirmed source loss; it is not a
        // reconnect/restart or an excuse to blame an unexplained helper exit.
        pulse.deadline(Instant::now() + OPERATION_TIMEOUT);
        match pulse.source(&open.source) {
            Err(error @ (AudioError::SourceMissing { .. } | AudioError::SourceChanged { .. })) => {
                result = Err(error)
            }
            Ok(current) => {
                if let Err(error) = same_source(&source, &current) {
                    result = Err(error);
                }
            }
            Err(_) => {}
        }
    }
    let cleanup = child.retire(shared);
    // The PA context is retired on this worker after real helper reaping.
    drop(pulse);
    match (result, cleanup) {
        (_, Err(error)) => Err(error),
        (Err(AudioError::Cancelled), Ok(())) if shared.cancelled() => Ok(()),
        (result, Ok(())) => result,
    }
}

fn run_child(
    open: &LoopbackOpen,
    names: &Names,
    shared: &Shared,
    pulse: &mut LoopbackPulse,
    source: &NativeSourceSnapshot,
    child: &mut OwnedChild,
) -> Result<(), AudioError> {
    let startup_deadline = Instant::now() + OPERATION_TIMEOUT;
    pulse.deadline(startup_deadline);
    let (capture, playback) = loop {
        if shared.cancelled() {
            return Ok(());
        }
        same_source(source, &pulse.source(&open.source)?)?;
        child.alive()?;
        let capture = attributed(pulse.streams(true, None)?, names, true, child.pid())?;
        let playback = attributed(pulse.streams(false, None)?, names, false, child.pid())?;
        if let (Some(capture), Some(playback)) = (capture, playback) {
            if capture.target == source.pulse_index && playback.target == INVALID_INDEX {
                break (stream_receipt(&capture), stream_receipt(&playback));
            }
            if playback.target != INVALID_INDEX {
                return Err(AudioError::Control(
                    "playback linked before controlled admission".into(),
                ));
            }
            if capture.target != INVALID_INDEX && capture.target != source.pulse_index {
                return Err(AudioError::SourceChanged {
                    name: open.source.name().into(),
                });
            }
        }
        if Instant::now() >= startup_deadline {
            return Err(AudioError::Control(
                "unlinked loopback startup timed out".into(),
            ));
        }
        thread::sleep(POLL);
    };
    pulse.bind_streams(capture.index, playback.index);
    if playback.global_id.is_none() {
        return Err(AudioError::MissingGlobalId {
            node: playback.node.clone(),
        });
    }
    let mut desired = shared.desired.lock().clone();
    confirm_mute(pulse, names, child, &playback, true)?;
    confirm_gain(pulse, names, child, &playback, desired.gain)?;
    check_capture(pulse, names, child, source, &capture)?;
    let current = fresh_stream(pulse, names, child, false, &playback)?;
    if current.target != INVALID_INDEX {
        return Err(AudioError::Control(
            "parking target was not unlinked after controls".into(),
        ));
    }
    shared.publish(LoopbackEvent::UnlinkedReady(receipt(
        open,
        source,
        child,
        (&capture, &playback),
        None,
        &desired,
        true,
    )));
    pulse.clear_deadline();
    let mut applied = 0;
    let mut destination: Option<LiveSinkTarget> = None;
    let mut destination_revision = None;
    let mut lost_revision = None;
    loop {
        if shared.cancelled() {
            return Ok(());
        }
        pulse.drain()?;
        if pulse.source_removed() {
            return Err(AudioError::SourceMissing {
                name: open.source.name().into(),
            });
        }
        child.alive()?;
        let dirty = pulse.take_dirty();
        {
            let newest = shared.desired.lock();
            if newest.serial != desired.serial {
                desired = newest.clone();
            }
        }
        if desired.serial == applied && !dirty {
            thread::sleep(POLL);
            continue;
        }
        let operation_deadline = Instant::now() + OPERATION_TIMEOUT;
        pulse.deadline(operation_deadline);
        check_capture(pulse, names, child, source, &capture)?;
        let playback_row = fresh_stream(pulse, names, child, false, &playback)?;
        // Subscription changes include our own mute/gain/move readbacks. An
        // unchanged, still-confirmed route needs observation, not another mute
        // transaction (which would cause an endless self-notification loop).
        if desired.serial == applied {
            let route_current = match &destination {
                Some(target) => {
                    !pulse.sink_removed(target.pulse_index)
                        && fresh_target(pulse, target)?
                        && playback_row.target == target.pulse_index
                }
                None => playback_row.muted,
            };
            let expected_mute = effective_mute(
                desired.gain.muted,
                destination.is_none(),
                false,
                shared.paused.load(Ordering::Acquire),
            );
            if route_current {
                let value = native_volume(desired.gain);
                if playback_row.muted != expected_mute
                    || playback_row.volumes.is_empty()
                    || playback_row
                        .volumes
                        .iter()
                        .any(|actual| actual.abs_diff(value) > 1)
                {
                    confirm_mute(pulse, names, child, &playback, true)?;
                    return Err(AudioError::Control(
                        "owned playback properties changed outside current intent".into(),
                    ));
                }
                pulse.clear_deadline();
                pulse.consume_sink_removals();
                thread::sleep(POLL);
                continue;
            }
        }
        // Every accepted output intent is one mute -> readback -> gain -> move
        // -> route observation -> newest-intent/source -> effective mute step.
        confirm_mute(pulse, names, child, &playback, true)?;
        confirm_gain(pulse, names, child, &playback, desired.gain)?;
        if destination
            .as_ref()
            .is_some_and(|target| pulse.sink_removed(target.pulse_index))
        {
            lost_revision = destination_revision;
            destination = None;
            destination_revision = None;
        }
        match &desired.output {
            OutputPlan::Silent { reason, .. } => {
                destination = None;
                destination_revision = None;
                if shared.current(&desired) {
                    shared.publish(LoopbackEvent::Silent {
                        reason: reason.clone(),
                        receipt: receipt(
                            open,
                            source,
                            child,
                            (&capture, &playback),
                            None,
                            &desired,
                            true,
                        ),
                    });
                    applied = desired.serial;
                }
            }
            OutputPlan::Target { revision, target } => {
                let latched = output_latched(*revision, lost_revision);
                if latched || !fresh_target(pulse, target)? {
                    lost_revision = Some(*revision);
                    destination = None;
                    destination_revision = None;
                    if shared.current(&desired) {
                        shared.publish(LoopbackEvent::Silent {
                            reason: OutputSilence::NoAvailableOutput,
                            receipt: receipt(
                                open,
                                source,
                                child,
                                (&capture, &playback),
                                None,
                                &desired,
                                true,
                            ),
                        });
                        applied = desired.serial;
                    }
                } else {
                    if !shared.current(&desired) {
                        pulse.clear_deadline();
                        continue;
                    }
                    shared.publish(LoopbackEvent::Switching {
                        revision: *revision,
                    });
                    let current = fresh_stream(pulse, names, child, false, &playback)?;
                    // An external policy move is a reported routing conflict,
                    // not permission to fight the policy in an automatic loop.
                    if desired.serial == applied
                        && playback_row.target != INVALID_INDEX
                        && destination
                            .as_ref()
                            .is_some_and(|old| playback_row.target != old.pulse_index)
                    {
                        return Err(AudioError::Control(
                            "owned output was moved by another policy".into(),
                        ));
                    }
                    if current.target != target.pulse_index {
                        if !fresh_target(pulse, target)? {
                            pulse.clear_deadline();
                            continue;
                        }
                        if !write_owned_target(
                            pulse,
                            names,
                            child,
                            source,
                            (&capture, &playback),
                            target,
                            (shared, &desired, operation_deadline),
                        )? {
                            pulse.clear_deadline();
                            continue;
                        }
                    }
                    loop {
                        if !shared.current(&desired) {
                            break;
                        }
                        check_capture(pulse, names, child, source, &capture)?;
                        if !fresh_target(pulse, target)? {
                            lost_revision = Some(*revision);
                            destination = None;
                            destination_revision = None;
                            confirm_mute(pulse, names, child, &playback, true)?;
                            if shared.current(&desired) {
                                shared.publish(LoopbackEvent::Silent {
                                    reason: OutputSilence::NoAvailableOutput,
                                    receipt: receipt(
                                        open,
                                        source,
                                        child,
                                        (&capture, &playback),
                                        None,
                                        &desired,
                                        true,
                                    ),
                                });
                                applied = desired.serial;
                            }
                            break;
                        }
                        let current = fresh_stream(pulse, names, child, false, &playback)?;
                        if current.target == target.pulse_index {
                            // No PA move is ever issued on this context, so its
                            // peer field has no issuing-client optimistic cache.
                            // Metadata helper exit/stdout is NOT route evidence.
                            check_capture(pulse, names, child, source, &capture)?;
                            if !shared.current(&desired) {
                                break;
                            }
                            let muted = effective_mute(
                                desired.gain.muted,
                                false,
                                false,
                                shared.paused.load(Ordering::Acquire),
                            );
                            let Some(observed) = confirm_admitted_mute(
                                pulse, names, child, &playback, muted, shared, &desired,
                            )?
                            else {
                                break;
                            };
                            check_capture(pulse, names, child, source, &capture)?;
                            if observed.target != target.pulse_index
                                || !fresh_target(pulse, target)?
                            {
                                confirm_mute(pulse, names, child, &playback, true)?;
                                return Err(AudioError::Control(
                                    "route changed during final mute readback".into(),
                                ));
                            }
                            if shared.current(&desired) {
                                destination = Some(target.clone());
                                lost_revision = None;
                                destination_revision = Some(*revision);
                                shared.publish(LoopbackEvent::Active(receipt(
                                    open,
                                    source,
                                    child,
                                    (&capture, &playback),
                                    destination.clone(),
                                    &desired,
                                    muted,
                                )));
                                applied = desired.serial;
                            } else {
                                confirm_mute(pulse, names, child, &playback, true)?;
                            }
                            break;
                        }
                        if Instant::now() >= operation_deadline {
                            return Err(AudioError::Control(
                                "output route observation timed out".into(),
                            ));
                        }
                        thread::sleep(POLL);
                    }
                }
            }
        }
        pulse.consume_sink_removals();
        pulse.clear_deadline();
        thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    fn names() -> Names {
        Names {
            app: "tag".into(),
            capture: "capture".into(),
            playback: "playback".into(),
            parking: "parking".into(),
        }
    }
    fn stream(index: u32, serial: u64, node: &str, pid: &str) -> LoopStream {
        let generated = format!(
            "{}.furami-loopback-{pid}",
            if node == "capture" { "input" } else { "output" }
        );
        LoopStream {
            index,
            serial: Some(serial),
            global_id: Some(55),
            node: Some(generated),
            app: Some("tag".into()),
            media: Some(node.into()),
            pid: Some(pid.into()),
            target: INVALID_INDEX,
            volumes: vec![65536, 65536],
            muted: false,
            volume_writable: true,
        }
    }
    #[test]
    fn ownership_uses_actual_child_pid_and_node_not_pa_client_index() {
        let rows = [stream(8, 19, "playback", "42")];
        assert!(
            attributed(rows.to_vec(), &names(), false, 42)
                .unwrap()
                .is_some()
        );
        assert!(attributed(rows.to_vec(), &names(), false, 43).is_err());
        let mut partial = rows[0].clone();
        partial.node = Some("foreign".into());
        assert!(attributed(vec![partial], &names(), false, 42).is_err());
    }
    #[test]
    fn same_index_replacement_cannot_inherit_owned_stream_authority() {
        let initial = stream(8, 19, "playback", "42");
        let receipt = stream_receipt(&initial);
        assert!(same_stream(&receipt, &initial));
        assert!(!same_stream(&receipt, &stream(8, 20, "playback", "42")));
        assert!(attributed(vec![initial.clone(), initial], &names(), false, 42).is_err());
    }
    #[test]
    fn owned_command_escapes_values_and_scrubs_only_helper_overrides() {
        let names = Names {
            app: "tag".into(),
            capture: "quote\"\\\n".into(),
            playback: "out".into(),
            parking: "missing".into(),
        };
        let command = child_command(Path::new(HELPER), &names, 999).unwrap();
        assert_eq!(command.get_program(), OsStr::new(HELPER));
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        let capture: serde_json::Value = serde_json::from_str(args[1]).unwrap();
        assert_eq!(capture["media.name"], names.capture);
        assert!(capture.get("node.name").is_none());
        assert_eq!(capture["target.object"], "999");
        assert_eq!(capture["node.dont-fallback"], "true");
        let playback: serde_json::Value = serde_json::from_str(args[3]).unwrap();
        assert_eq!(playback["target.object"], "missing");
        assert_eq!(playback["node.linger"], "true");
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "LD_LIBRARY_PATH" && value.is_none())
        );
        assert!(!command.get_envs().any(|(key, _)| key == "PULSE_SERVER"
            || key == "XDG_RUNTIME_DIR"
            || key == "PIPEWIRE_REMOTE"));
    }
    #[test]
    fn cubic_ui_response_is_encoded_once_and_all_silence_levels_dominate() {
        assert_eq!(native_volume(PlaybackGain::new(50, false).unwrap()), 32768);
        for levels in [
            [true, false, false, false],
            [false, true, false, false],
            [false, false, true, false],
            [false, false, false, true],
        ] {
            assert!(effective_mute(levels[0], levels[1], levels[2], levels[3]));
        }
        assert!(!effective_mute(false, false, false, false));
    }

    fn mailbox_owner() -> LoopbackOwner {
        LoopbackOwner {
            shared: Arc::new(Shared {
                stop: Arc::new(AtomicBool::new(false)),
                owner_cancel: Arc::new(AtomicBool::new(false)),
                paused: AtomicBool::new(false),
                desired: Mutex::new(Desired {
                    gain: PlaybackGain::default(),
                    output: OutputPlan::Silent {
                        revision: OutputRevision::first(),
                        reason: OutputSilence::ManualRequiresAction,
                    },
                    serial: 1,
                }),
                event: Mutex::new(None),
            }),
            worker: None,
            joined: false,
        }
    }

    #[test]
    fn latest_gain_coalesces_without_clearing_output_latch_and_stop_dominates() {
        let mut owner = mailbox_owner();
        for percent in 0..=100 {
            owner
                .submit(LoopbackCommand::SetGain(
                    PlaybackGain::new(percent, false).unwrap(),
                ))
                .unwrap();
        }
        let desired = owner.shared.desired.lock().clone();
        assert_eq!(desired.gain.volume_percent, 100);
        assert!(matches!(
            desired.output,
            OutputPlan::Silent {
                reason: OutputSilence::ManualRequiresAction,
                ..
            }
        ));
        owner.submit(LoopbackCommand::Pause).unwrap();
        assert!(owner.shared.paused.load(Ordering::Acquire));
        assert_eq!(
            owner.submit(LoopbackCommand::SetGain(
                PlaybackGain::new(1, false).unwrap()
            )),
            Err(AudioError::Cancelled)
        );
        // A requested stop is not a joined/reaped acknowledgment.
        assert!(owner.try_join().is_none());
    }

    #[test]
    fn stale_output_cannot_replace_a_newer_silence_intent() {
        let owner = mailbox_owner();
        let old = OutputRevision::first();
        let new = old.next();
        let plan = OutputPlan::Silent {
            revision: new,
            reason: OutputSilence::ManualUnavailable,
        };
        owner
            .submit(LoopbackCommand::SetOutput(plan.clone()))
            .unwrap();
        assert!(
            owner
                .submit(LoopbackCommand::SetOutput(OutputPlan::Silent {
                    revision: old,
                    reason: OutputSilence::NoAvailableOutput,
                }))
                .is_err()
        );
        assert_eq!(owner.shared.desired.lock().output, plan);
    }

    #[test]
    fn retirement_deadlines_escalate_but_never_turn_live_child_into_ack() {
        assert_eq!(
            retirement_stage(Duration::ZERO, false),
            RetirementStage::Term
        );
        assert_eq!(retirement_stage(TERM_TIMEOUT, false), RetirementStage::Kill);
        assert_eq!(
            retirement_stage(TERM_TIMEOUT + KILL_TIMEOUT, false),
            RetirementStage::Blocked
        );
        assert_eq!(
            retirement_stage(TERM_TIMEOUT + KILL_TIMEOUT, true),
            RetirementStage::Reaped
        );
    }

    #[test]
    fn owned_child_stop_reaps_the_actual_process() {
        let owner = mailbox_owner();
        let mut command = Command::new("/usr/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = OwnedChild::spawn(&mut command).unwrap();
        child.retire(&owner.shared).unwrap();
        assert!(child.reaped);
        assert!(child.child.try_wait().unwrap().is_some());
    }

    #[test]
    fn generated_pid_name_is_required_even_when_process_property_is_absent() {
        let mut native = stream(8, 19, "playback", "42");
        native.pid = None;
        assert!(
            attributed(vec![native.clone()], &names(), false, 42)
                .unwrap()
                .is_some()
        );
        native.node = Some("output.furami-loopback-43".into());
        assert!(attributed(vec![native], &names(), false, 42).is_err());
        assert!(!generated_node(
            Some("output.furami-loopback-+42"),
            false,
            42
        ));
        assert!(!generated_node(
            Some("output.furami-loopback-042"),
            false,
            42
        ));
    }

    #[test]
    fn newer_mute_or_silence_during_lookup_denies_actual_unmute_admission() {
        let owner = mailbox_owner();
        let before_lookup = owner.shared.desired.lock().clone();
        owner
            .submit(LoopbackCommand::SetGain(
                PlaybackGain::new(100, true).unwrap(),
            ))
            .unwrap();
        assert!(owner.shared.admission(&before_lookup).is_none());
        let after_mute = owner.shared.desired.lock().clone();
        assert!(owner.shared.admission(&after_mute).is_some());
        owner
            .submit(LoopbackCommand::SetOutput(OutputPlan::Silent {
                revision: OutputRevision::first().next(),
                reason: OutputSilence::ManualUnavailable,
            }))
            .unwrap();
        assert!(owner.shared.admission(&after_mute).is_none());
        owner.stop();
        let after_silence = owner.shared.desired.lock().clone();
        assert!(owner.shared.admission(&after_silence).is_none());
    }

    #[test]
    fn removal_of_applied_a_does_not_latch_new_desired_b() {
        let applied_a = OutputRevision::first();
        let desired_b = applied_a.next();
        assert!(output_latched(applied_a, Some(applied_a)));
        assert!(!output_latched(desired_b, Some(applied_a)));
        assert!(!output_latched(desired_b, None));
    }

    #[test]
    fn metadata_subject_uses_observed_global_id_not_pulse_index_or_serial() {
        let row = stream(8, 19, "playback", "42");
        assert_eq!(metadata_subject(&row).unwrap(), 55);
        let (command, _) = metadata_command(metadata_subject(&row).unwrap(), 1234).unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(
            args,
            ["-n", "default", "55", "target.object", "1234", "Spa:Id"]
        );
        assert_eq!(command.get_program(), OsStr::new(METADATA_HELPER));
        assert!(!args.contains(&"-m"));
    }

    #[test]
    fn missing_or_core_global_id_has_typed_failure_without_index_conversion() {
        for global_id in [None, Some(0), Some(INVALID_INDEX)] {
            let mut row = stream(8, 19, "playback", "42");
            row.global_id = global_id;
            assert!(matches!(
                metadata_subject(&row),
                Err(AudioError::MissingGlobalId { .. })
            ));
        }
        assert!(metadata_command(0, 1234).is_err());
        assert!(metadata_command(INVALID_INDEX, 1234).is_err());
    }

    #[test]
    fn native_subject_replacement_invalidates_the_same_index_and_serial() {
        let initial = stream(8, 19, "playback", "42");
        let expected = stream_receipt(&initial);
        let mut changed = initial;
        changed.global_id = Some(56);
        assert!(!same_stream(&expected, &changed));
    }

    #[test]
    fn metadata_stderr_capture_is_nonblocking_and_byte_bounded() {
        use std::io::Write as _;
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut bytes = Vec::with_capacity(MAX_METADATA_STDERR);
        let mut truncated = false;
        drain_metadata_stderr(&mut reader, &mut bytes, &mut truncated).unwrap();
        assert!(bytes.is_empty());
        writer
            .write_all(&vec![b'x'; MAX_METADATA_STDERR + 1024])
            .unwrap();
        drain_metadata_stderr(&mut reader, &mut bytes, &mut truncated).unwrap();
        assert_eq!(bytes.len(), MAX_METADATA_STDERR);
        assert!(truncated);
    }
}
