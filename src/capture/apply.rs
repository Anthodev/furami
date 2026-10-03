//! Concrete draft validation adapter: full prevalidation of identity, mode,
//! route and enabled audio before any live teardown, on a single worker.
//! Allowed dependencies: `app` ports, `domain`, owned capture primitives.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, TryRecvError},
    },
    thread::JoinHandle,
};

use crate::{
    app::ports::{DraftValidator, SubmitFailure, ValidationResult},
    capture::{
        audio::{self, AudioSource},
        input::{CaptureSelection, InputError, InputSpec, SelectionError},
        linux::{self, CaptureError, CaptureSnapshot, ValidatedCapture},
    },
    domain::capture::CaptureBufferType,
    domain::{
        capture::{AudioError, AudioSelection, AudioSourceIdentity},
        failure::{ApplyFailure, Cause, FailureCategory, Stage, ValidationLayer},
        state::{DraftSettings, ValidationKey, ValidationRequest},
    },
};

/// Fully proven capture plan. Built only after identity, exact mode tuple,
/// unique single-planar route and enabled audio all revalidated against one
/// fresh observation pass. No prepared value exists for any failed layer.
#[derive(Debug)]
pub struct PreparedCapture {
    settings: DraftSettings,
    selection: CaptureSelection,
    input: InputSpec,
    audio: Option<AudioSourceIdentity>,
}

impl PreparedCapture {
    pub fn settings(&self) -> &DraftSettings {
        &self.settings
    }

    pub fn selection(&self) -> &CaptureSelection {
        &self.selection
    }

    pub fn input(&self) -> &InputSpec {
        &self.input
    }

    /// Exact requested identity of the enabled source. Disabled retains its
    /// missing source without any audio input being opened.
    pub fn audio(&self) -> Option<&AudioSourceIdentity> {
        self.audio.as_ref()
    }

    pub fn into_parts(
        self,
    ) -> (
        DraftSettings,
        CaptureSelection,
        InputSpec,
        Option<AudioSourceIdentity>,
    ) {
        (self.settings, self.selection, self.input, self.audio)
    }
}

/// Full validation of identity, exact mode tuple, unique single-planar route
/// and enabled audio against one observed snapshot pass. The exact-interval
/// query is read-only live I/O (`linux::query_intervals`): this is the real
/// worker/production path, not a fixture boundary. Deterministic tests inject
/// their exact-interval answers via the `#[cfg(test)]` seam below; no pure
/// no-hardware entry point exists by design.
pub fn validate_prepared(
    settings: DraftSettings,
    snapshot: &CaptureSnapshot,
    audio_catalog: &[AudioSource],
) -> Result<PreparedCapture, ApplyFailure> {
    validate_prepared_with(settings, snapshot, audio_catalog, linux::query_intervals)
}

/// Test seam: identical pipeline with an injected exact-interval query, so
/// range-descriptor fixtures stay deterministic and never open real nodes.
#[cfg(test)]
fn validate_prepared_fixture(
    settings: DraftSettings,
    snapshot: &CaptureSnapshot,
    audio_catalog: &[AudioSource],
    query: impl FnMut(
        &crate::capture::linux::CaptureDevice,
        &crate::capture::linux::CaptureNode,
        crate::domain::capture::CapturedFourCc,
        crate::domain::capture::FrameSize,
    ) -> Result<crate::capture::linux::ExactSizeIntervals, CaptureError>,
) -> Result<PreparedCapture, ApplyFailure> {
    validate_prepared_with(settings, snapshot, audio_catalog, query)
}

fn validate_prepared_with(
    settings: DraftSettings,
    snapshot: &CaptureSnapshot,
    audio_catalog: &[AudioSource],
    mut query: impl FnMut(
        &crate::capture::linux::CaptureDevice,
        &crate::capture::linux::CaptureNode,
        crate::domain::capture::CapturedFourCc,
        crate::domain::capture::FrameSize,
    ) -> Result<crate::capture::linux::ExactSizeIntervals, CaptureError>,
) -> Result<PreparedCapture, ApplyFailure> {
    let validated = linux::validate_with_injected_query(snapshot, &settings.video, &mut query)
        .map_err(|error| capture_failure(&settings, "validate requested tuple", &error))?;
    let node = unique_single_planar_node(&settings, &validated)?;
    let selection = CaptureSelection::from_snapshot(snapshot, node.as_path(), settings.video.mode)
        .map_err(|error| selection_failure(&settings, "resolve the selected route", &error))?;
    // Built from the same proven ValidatedCapture: one validation pass, no
    // second live interval query, identical route proof.
    let input = InputSpec::from_validated(&validated, Some(node.as_path()))
        .map_err(|error| input_failure(&settings, "construct the pinned capture input", &error))?;
    let audio = match &settings.audio {
        AudioSelection::Disabled { .. } => None,
        AudioSelection::Enabled { source } => {
            audio::validate_snapshot(source, audio_catalog).map_err(|error| {
                audio_failure(&settings, "revalidate the enabled audio source", &error)
            })?;
            Some(source.clone())
        }
    };
    Ok(PreparedCapture {
        settings,
        selection,
        input,
        audio,
    })
}

/// Exactly one fresh physical route for the requested tuple; ambiguity fails
/// and no cached `/dev` node is ever substituted.
fn unique_single_planar_node(
    settings: &DraftSettings,
    validated: &ValidatedCapture<'_>,
) -> Result<std::path::PathBuf, ApplyFailure> {
    let mut nodes: Vec<std::path::PathBuf> = validated
        .routes()
        .iter()
        .filter(|route| route.buffer_type() == CaptureBufferType::SinglePlanar)
        .map(|route| route.node().devnode().to_owned())
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    match nodes.as_slice() {
        [node] => Ok(node.clone()),
        [] => Err(ApplyFailure::new(
            FailureCategory::Validation(ValidationLayer::Input),
            Stage::InputConstruction,
            Cause::RequestedModeRefused,
            settings.clone(),
            "select the unique capture route",
            "requested tuple proven on no single-planar capture route",
        )),
        _ => Err(ApplyFailure::new(
            FailureCategory::Validation(ValidationLayer::Input),
            Stage::InputConstruction,
            Cause::RequestedModeRefused,
            settings.clone(),
            "select the unique capture route",
            format!(
                "requested tuple proven on multiple capture routes: {:?}",
                nodes
            ),
        )),
    }
}

fn capture_failure(
    requested: &DraftSettings,
    operation: &'static str,
    error: &CaptureError,
) -> ApplyFailure {
    let (layer, cause) = match error {
        CaptureError::PermissionDenied { .. } => (ValidationLayer::Input, Cause::Permission),
        CaptureError::DeviceGone { .. } | CaptureError::StaleSnapshot { .. } => {
            (ValidationLayer::Input, Cause::RequestedModeRefused)
        }
        CaptureError::Io { source, .. } => (
            ValidationLayer::Input,
            match source.raw_os_error() {
                // EBUSY: device busy; nothing else here proves busy-ness.
                Some(16) => Cause::Busy,
                _ => Cause::Generic,
            },
        ),
        CaptureError::InvalidNode { .. } => (ValidationLayer::Input, Cause::RequestedModeRefused),
        CaptureError::Identity(_) => (ValidationLayer::Identity, Cause::RequestedModeRefused),
        CaptureError::Unsupported(_) => (ValidationLayer::Mode, Cause::RequestedModeRefused),
        CaptureError::NoVideoNodes
        | CaptureError::NoCaptureNodes
        | CaptureError::UdevFailure { .. }
        | CaptureError::Udev { .. }
        | CaptureError::MissingDevnode { .. }
        | CaptureError::MissingUsbAttribute { .. }
        | CaptureError::InvalidUsbAttribute { .. }
        | CaptureError::MalformedDescriptor { .. }
        | CaptureError::Data(_)
        | CaptureError::DescriptorUnavailable(_) => (ValidationLayer::Discovery, Cause::Generic),
    };
    // No fabricated BackendEvidence: BackendOperation/errno evidence is
    // reserved for structured media-backend reports. The original errno stays
    // in the preserved diagnostic text of the capture error.
    ApplyFailure::new(
        FailureCategory::Validation(layer),
        Stage::Prevalidation,
        cause,
        requested.clone(),
        operation,
        error.to_string(),
    )
}

fn selection_failure(
    requested: &DraftSettings,
    operation: &'static str,
    error: &SelectionError,
) -> ApplyFailure {
    match error {
        SelectionError::Capture(inner) => capture_failure(requested, operation, inner),
        SelectionError::NodeNotFound(node) => ApplyFailure::new(
            FailureCategory::Validation(ValidationLayer::Input),
            Stage::Prevalidation,
            Cause::RequestedModeRefused,
            requested.clone(),
            operation,
            format!("selected node {node:?} is not an eligible discovered USB UVC capture node"),
        ),
        SelectionError::NodeAmbiguous(node) => ApplyFailure::new(
            FailureCategory::Validation(ValidationLayer::Input),
            Stage::Prevalidation,
            Cause::RequestedModeRefused,
            requested.clone(),
            operation,
            format!("selected node {node:?} belongs to multiple discovered routes"),
        ),
        SelectionError::Input(inner) => input_failure(requested, operation, inner),
    }
}

fn input_failure(
    requested: &DraftSettings,
    operation: &'static str,
    error: &InputError,
) -> ApplyFailure {
    let (layer, cause) = match error {
        InputError::RouteAmbiguous | InputError::RouteNotProven(_) => {
            (ValidationLayer::Input, Cause::RequestedModeRefused)
        }
        InputError::UnmappedFourCc(_)
        | InputError::MalformedSpec(_)
        | InputError::UnrepresentablePath => (ValidationLayer::Data, Cause::Generic),
    };
    ApplyFailure::new(
        FailureCategory::Validation(layer),
        Stage::InputConstruction,
        cause,
        requested.clone(),
        operation,
        error.to_string(),
    )
}

fn audio_failure(
    requested: &DraftSettings,
    operation: &'static str,
    error: &AudioError,
) -> ApplyFailure {
    let cause = match error {
        AudioError::InvalidSelection(_) => Cause::RequestedModeRefused,
        _ => Cause::Generic,
    };
    ApplyFailure::new(
        FailureCategory::Validation(ValidationLayer::Audio),
        Stage::Prevalidation,
        cause,
        requested.clone(),
        operation,
        error.to_string(),
    )
}

#[cfg(test)]
pub(crate) fn fixture_prepared(settings: DraftSettings) -> Result<PreparedCapture, ApplyFailure> {
    let snapshot = crate::capture::linux::session_fixture(&["/dev/video0"], settings.video.mode);
    validate_prepared(settings, &snapshot, &[])
}

enum Command {
    Validate(ValidationRequest),
    Stop,
}

type WorkerOutcome = (ValidationRequest, Result<PreparedCapture, ApplyFailure>);

fn worker_disconnected_failure(
    request: &ValidationRequest,
    diagnostic: &'static str,
) -> ApplyFailure {
    ApplyFailure::new(
        FailureCategory::Validation(ValidationLayer::Discovery),
        Stage::Prevalidation,
        Cause::Generic,
        request.settings.clone(),
        "run the draft validation to a terminal result",
        diagnostic,
    )
}

/// One validation at a time on a retired-on-shutdown worker. All client calls
/// are nonblocking and bounded; a cancelled request still produces its
/// terminal result with the original request before anything else drains.
pub struct CaptureValidator {
    tx: mpsc::SyncSender<Command>,
    rx: mpsc::Receiver<WorkerOutcome>,
    handle: Option<JoinHandle<()>>,
    retired: Arc<AtomicBool>,
    pending: Option<ValidationRequest>,
    ready: Option<ValidationResult<PreparedCapture>>,
    shutdown_requested: bool,
    stop_sent: bool,
    /// Set once an accepted request was drained via channel disconnect:
    /// further submits are refused because no result can ever arrive.
    worker_result_dead: bool,
}

fn run_validation(request: ValidationRequest) -> WorkerOutcome {
    let outcome = (|| {
        let snapshot = linux::discover().map_err(|error| {
            capture_failure(&request.settings, "discover capture devices", &error)
        })?;
        let audio_catalog = if request.settings.audio.enabled() {
            audio::discover().map_err(|error| {
                audio_failure(&request.settings, "discover audio sources", &error)
            })?
        } else {
            // Disabled retains its missing source: no catalog read, no input.
            Vec::new()
        };
        validate_prepared(request.settings.clone(), &snapshot, &audio_catalog)
    })();
    (request, outcome)
}

impl CaptureValidator {
    pub fn new() -> Self {
        Self::with_runner(run_validation)
    }

    /// Test seam: deterministic fixture runner, identical worker semantics.
    pub fn with_runner(execute: fn(ValidationRequest) -> WorkerOutcome) -> Self {
        // Bounded at 2: at most one Validate (slot guard) plus one Stop can
        // ever be queued, so Stop submission can never wedge behind Full.
        let (tx, command_rx) = mpsc::sync_channel(2);
        let (outcome_tx, rx) = mpsc::channel();
        let retired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&retired);
        let spawned = std::thread::Builder::new()
            .name("furami-draft-validation".into())
            .spawn(move || {
                while let Ok(command) = command_rx.recv() {
                    match command {
                        Command::Validate(request) => {
                            // A panicking runner must still yield a terminal
                            // result for the accepted request.
                            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                                || execute(request.clone()),
                            ))
                            .unwrap_or_else(|_| {
                                let failure = worker_disconnected_failure(
                                    &request,
                                    "validation worker panicked before delivering its terminal result",
                                );
                                (request, Err(failure))
                            });
                            // Exact prevalidation-pass evidence lives with the
                            // capture adapter; runtime only consumes the port.
                            let payload = match &outcome.1 {
                                Ok(_) => serde_json::json!({
                                    "request": outcome.0,
                                    "status": "Validated",
                                    "failure": null,
                                }),
                                Err(failure) => serde_json::json!({
                                    "request": outcome.0,
                                    "status": "Failed",
                                    "failure": failure,
                                }),
                            };
                            tracing::info!(
                                apply_validation = %payload,
                                "apply_validation"
                            );
                            if outcome_tx.send(outcome).is_err() {
                                break;
                            }
                        }
                        Command::Stop => break,
                    }
                }
                flag.store(true, Ordering::SeqCst);
            });
        match spawned {
            Ok(handle) => Self {
                tx,
                rx,
                handle: Some(handle),
                retired,
                pending: None,
                ready: None,
                shutdown_requested: false,
                stop_sent: false,
                worker_result_dead: false,
            },
            // No worker: permanently disconnected validator, no panic.
            Err(_) => Self {
                tx,
                rx,
                handle: None,
                retired: Arc::new(AtomicBool::new(true)),
                pending: None,
                ready: None,
                shutdown_requested: false,
                stop_sent: false,
                worker_result_dead: true,
            },
        }
    }
}

impl Default for CaptureValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl DraftValidator for CaptureValidator {
    type Prepared = PreparedCapture;

    fn begin_validate(&mut self, request: ValidationRequest) -> Result<(), SubmitFailure> {
        if self.shutdown_requested || self.worker_result_dead || self.retired.load(Ordering::SeqCst)
        {
            return Err(SubmitFailure::Disconnected);
        }
        if self.pending.is_some() || self.ready.is_some() {
            // Exactly one request/result slot until the result is consumed.
            return Err(SubmitFailure::CapacityUnavailable);
        }
        self.tx
            .try_send(Command::Validate(request.clone()))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => SubmitFailure::CapacityUnavailable,
                mpsc::TrySendError::Disconnected(_) => SubmitFailure::Disconnected,
            })?;
        self.pending = Some(request);
        Ok(())
    }

    fn poll_validation(&mut self) -> Option<ValidationResult<Self::Prepared>> {
        if self.ready.is_none() && self.pending.is_some() {
            match self.rx.try_recv() {
                Ok((request, result)) => {
                    self.ready = Some(ValidationResult { request, result });
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    // The worker died without delivering a result: the
                    // accepted request still gets a terminal failure with its
                    // original request, the slot drains, and further submits
                    // are refused (no result channel can ever answer again).
                    self.worker_result_dead = true;
                    if let Some(pending) = self.pending.take() {
                        self.ready = Some(ValidationResult {
                            request: pending.clone(),
                            result: Err(worker_disconnected_failure(
                                &pending,
                                "validation worker disconnected before delivering its terminal result",
                            )),
                        });
                    }
                }
            }
        }
        let result = self.ready.take();
        if result.is_some() {
            self.pending = None;
        }
        result
    }

    fn cancel_validation(&mut self, key: ValidationKey) {
        // In-flight ioctl work cannot be interrupted; the request still runs to
        // its terminal result with the original request, which the caller then
        // observes via poll_validation before the slot drains.
        if let Some(pending) = &self.pending {
            debug_assert_eq!(pending.key, key, "only one request can be cancelled");
        }
    }

    fn shutdown(&mut self) {
        self.shutdown_requested = true;
        // Level-triggered: at most one Stop is ever enqueued.
        // Level-triggered with retry: stop_sent is committed only once a Stop
        // is actually queued. Bounded(2) makes Full impossible for Stop here.
        if !self.stop_sent && self.tx.try_send(Command::Stop).is_ok() {
            self.stop_sent = true;
        }
    }

    /// True only once the worker thread has actually retired. Never times out;
    /// join is gated on thread exit so this call never blocks.
    fn shutdown_complete(&mut self) -> bool {
        if !self.shutdown_requested {
            return false;
        }
        if let Some(handle) = &self.handle
            && !handle.is_finished()
        {
            return false;
        }
        if let Some(handle) = self.handle.take() {
            // Thread already exited; join reaps without blocking.
            let _ = handle.join();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capture::linux::session_fixture,
        domain::{
            capture::{CapturedFourCc, DeviceIdentity, FrameRate, FrameSize, ModeRequest},
            state::{ApplyId, AttemptPurpose, DraftRevision, ValidationKey},
        },
    };
    use std::path::Path;
    use std::sync::OnceLock;
    use std::sync::mpsc;

    fn mode() -> crate::domain::capture::CaptureMode {
        crate::domain::capture::CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(2560, 1440).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        }
    }

    fn fixture_identity() -> DeviceIdentity {
        session_fixture(&["/dev/video0"], mode()).devices()[0]
            .identity()
            .clone()
    }

    fn settings(
        video_identity: DeviceIdentity,
        mode: crate::domain::capture::CaptureMode,
        audio: AudioSelection,
    ) -> DraftSettings {
        DraftSettings {
            video: ModeRequest {
                identity: video_identity,
                mode,
            },
            audio,
        }
    }

    fn request(settings: DraftSettings) -> ValidationRequest {
        ValidationRequest {
            key: ValidationKey {
                apply: ApplyId::new(1).unwrap(),
                purpose: AttemptPurpose::Candidate,
            },
            revision: DraftRevision::default(),
            settings,
        }
    }

    fn source(name: &str, card: &str) -> AudioSource {
        AudioSource {
            identity: crate::domain::capture::AudioSourceIdentity::new(
                name.into(),
                vec![("card.name".into(), card.into())],
            )
            .unwrap(),
            description: "fixture source".into(),
        }
    }

    fn fixture_audio() -> Vec<AudioSource> {
        vec![source("fixture-source", "K7")]
    }

    #[test]
    fn supported_exact_tuple_prepares_the_requested_route() {
        let mode = mode();
        let snapshot = session_fixture(&["/dev/video0"], mode);
        let prepared = validate_prepared(
            settings(fixture_identity(), mode, AudioSelection::default()),
            &snapshot,
            &fixture_audio(),
        )
        .unwrap();
        let requested = prepared.input().requested();
        assert_eq!(requested.identity, fixture_identity());
        assert_eq!(requested.mode, mode);
        assert_eq!(prepared.input().node(), Path::new("/dev/video0"));
        assert_eq!(prepared.input().url().to_bytes(), b"av://v4l2:/dev/video0");
        assert!(prepared.audio().is_none());
    }

    #[test]
    fn invalid_layers_produce_no_prepared_value() {
        let mode = mode();
        let snapshot = session_fixture(&["/dev/video0"], mode);
        let missing_identity = DeviceIdentity::new(
            0x1111,
            0x2222,
            crate::domain::capture::UsbTopology::new(
                "pci-other".into(),
                vec![std::num::NonZeroU8::new(7).unwrap()],
            )
            .unwrap(),
            None,
        )
        .unwrap();
        // Identity layer.
        let identity_failure = validate_prepared(
            settings(missing_identity, mode, AudioSelection::default()),
            &snapshot,
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            identity_failure.category,
            FailureCategory::Validation(ValidationLayer::Identity)
        ));
        // Mode layer: unsupported frame rate for the exact tuple.
        let mut unsupported = mode;
        unsupported.rate = FrameRate::new(30, 1).unwrap();
        let mode_failure = validate_prepared(
            settings(fixture_identity(), unsupported, AudioSelection::default()),
            &snapshot,
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            mode_failure.category,
            FailureCategory::Validation(ValidationLayer::Mode)
        ));
        // Audio layer: enabled source absent from the fresh catalog.
        let audio_failure = validate_prepared(
            settings(
                fixture_identity(),
                mode,
                AudioSelection::Enabled {
                    source: source("missing", "gone").identity,
                },
            ),
            &snapshot,
            &fixture_audio(),
        )
        .unwrap_err();
        assert!(matches!(
            audio_failure.category,
            FailureCategory::Validation(ValidationLayer::Audio)
        ));
    }

    #[test]
    fn ambiguous_routes_fail_closed() {
        let mode = mode();
        let snapshot = session_fixture(&["/dev/video0", "/dev/video1"], mode);
        let failure = validate_prepared(
            settings(fixture_identity(), mode, AudioSelection::default()),
            &snapshot,
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            failure.category,
            FailureCategory::Validation(ValidationLayer::Input)
        ));
        assert!(failure.diagnostic.contains("multiple capture routes"));
    }

    #[test]
    fn renumbered_route_resolves_fresh_without_cached_node() {
        let mode = mode();
        let original = session_fixture(&["/dev/video0"], mode);
        let prepared = validate_prepared(
            settings(fixture_identity(), mode, AudioSelection::default()),
            &original,
            &[],
        )
        .unwrap();
        assert_eq!(prepared.input().node(), Path::new("/dev/video0"));
        // Same physical identity reappears renumbered: fresh resolution follows
        // it; no cached /dev/video0 fallback is reused.
        let renumbered = session_fixture(&["/dev/video9"], mode);
        let prepared = validate_prepared(
            settings(fixture_identity(), mode, AudioSelection::default()),
            &renumbered,
            &[],
        )
        .unwrap();
        assert_eq!(prepared.input().node(), Path::new("/dev/video9"));
    }

    #[test]
    fn missing_route_after_renumber_fails() {
        let mode = mode();
        let gone = session_fixture(&["/dev/video9"], mode);
        let stale = DeviceIdentity::new(
            0x3333,
            0x4444,
            crate::domain::capture::UsbTopology::new(
                "pci-elsewhere".into(),
                vec![std::num::NonZeroU8::new(3).unwrap()],
            )
            .unwrap(),
            Some("fixture".into()),
        )
        .unwrap();
        let failure =
            validate_prepared(settings(stale, mode, AudioSelection::default()), &gone, &[])
                .unwrap_err();
        assert!(matches!(
            failure.category,
            FailureCategory::Validation(ValidationLayer::Identity)
        ));
    }

    #[test]
    fn disabled_retains_missing_source_without_audio_input() {
        let mode = mode();
        let snapshot = session_fixture(&["/dev/video0"], mode);
        let retained = source("vanished", "K7").identity;
        let prepared = validate_prepared(
            settings(
                fixture_identity(),
                mode,
                AudioSelection::Disabled {
                    retained: Some(retained),
                },
            ),
            &snapshot,
            // Empty catalog: disabled validation never consults it.
            &[],
        )
        .unwrap();
        assert!(prepared.audio().is_none());
        assert!(prepared.settings().audio.source().is_some());
    }

    #[test]
    fn changed_audio_properties_fail_revalidation() {
        let mode = mode();
        let snapshot = session_fixture(&["/dev/video0"], mode);
        let failure = validate_prepared(
            settings(
                fixture_identity(),
                mode,
                AudioSelection::Enabled {
                    source: source("fixture-source", "REPLACED-CARD").identity,
                },
            ),
            &snapshot,
            &fixture_audio(),
        )
        .unwrap_err();
        assert!(matches!(
            failure.category,
            FailureCategory::Validation(ValidationLayer::Audio)
        ));
    }

    #[test]
    fn capture_io_errors_keep_cause_layer_and_errno_evidence() {
        let requested = settings(fixture_identity(), mode(), AudioSelection::default());
        let source = std::io::Error::from_raw_os_error(13); // EACCES
        let error = CaptureError::PermissionDenied {
            path: "/dev/video0".into(),
            operation: "open",
            source,
        };
        let failure = capture_failure(&requested, "probe", &error);
        assert_eq!(failure.cause, Cause::Permission);
        assert!(matches!(
            failure.category,
            FailureCategory::Validation(ValidationLayer::Input)
        ));
        assert!(failure.evidence.is_none());
        assert!(failure.diagnostic.contains("Permission denied"));
        let busy = CaptureError::Io {
            path: "/dev/video0".into(),
            operation: "querycap",
            source: std::io::Error::from_raw_os_error(16), // EBUSY
        };
        let failure = capture_failure(&requested, "probe", &busy);
        assert_eq!(failure.cause, Cause::Busy);
    }

    fn fixture_runner() -> fn(ValidationRequest) -> WorkerOutcome {
        fn execute(request: ValidationRequest) -> WorkerOutcome {
            let snapshot = session_fixture(&["/dev/video0"], request.settings.video.mode);
            let outcome = validate_prepared(request.settings.clone(), &snapshot, &fixture_audio());
            (request, outcome)
        }
        execute
    }

    fn wait_result(validator: &mut CaptureValidator) -> ValidationResult<PreparedCapture> {
        for _ in 0..2000 {
            if let Some(result) = validator.poll_validation() {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("validation worker never produced its terminal result");
    }

    fn wait_shutdown(validator: &mut CaptureValidator) -> bool {
        for _ in 0..2000 {
            if validator.shutdown_complete() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn worker_delivers_one_terminal_result_per_request() {
        let mut validator = CaptureValidator::with_runner(fixture_runner());
        let first = request(settings(
            fixture_identity(),
            mode(),
            AudioSelection::default(),
        ));
        validator.begin_validate(first.clone()).unwrap();
        // Exactly one slot: a second submit while one is outstanding is refused.
        assert_eq!(
            validator.begin_validate(first.clone()),
            Err(SubmitFailure::CapacityUnavailable)
        );
        let result = wait_result(&mut validator);
        assert_eq!(result.request.key.apply, first.key.apply);
        assert!(result.result.is_ok());
        // Consumed slot accepts a new request.
        validator.begin_validate(first.clone()).unwrap();
        assert!(wait_result(&mut validator).result.is_ok());
        validator.shutdown();
        assert!(wait_shutdown(&mut validator));
    }

    #[test]
    fn cancelled_request_still_yields_original_terminal_result() {
        let mut validator = CaptureValidator::with_runner(fixture_runner());
        let original = request(settings(
            fixture_identity(),
            mode(),
            AudioSelection::default(),
        ));
        validator.begin_validate(original.clone()).unwrap();
        validator.cancel_validation(original.key);
        let result = wait_result(&mut validator);
        assert_eq!(result.request.key.apply, original.key.apply);
        assert_eq!(result.request.key.purpose, original.key.purpose);
        validator.shutdown();
        assert!(wait_shutdown(&mut validator));
    }

    #[test]
    fn shutdown_refuses_new_work_and_completes_only_on_retirement() {
        let mut validator = CaptureValidator::with_runner(fixture_runner());
        assert!(!validator.shutdown_complete());
        validator.shutdown();
        assert_eq!(
            validator.begin_validate(request(settings(
                fixture_identity(),
                mode(),
                AudioSelection::default()
            ))),
            Err(SubmitFailure::Disconnected)
        );
        assert!(wait_shutdown(&mut validator));
        assert!(validator.shutdown_complete());
    }

    #[test]
    fn shutdown_queued_behind_in_flight_request_is_honored() {
        static RUNNER_START_TX: OnceLock<mpsc::Sender<()>> = OnceLock::new();
        static RUNNER_GATE_RX: OnceLock<std::sync::Mutex<mpsc::Receiver<()>>> = OnceLock::new();

        fn execute(request: ValidationRequest) -> WorkerOutcome {
            // Park only the gated validation (ApplyId 2) until released.
            if request.key.apply.get() == 2 {
                RUNNER_START_TX.get().unwrap().send(()).unwrap();
                // Gate stays closed until the test's explicit send; only a
                // dropped sender (test unwind/failure) releases the worker.
                match RUNNER_GATE_RX.get().unwrap().lock().unwrap().recv() {
                    Ok(()) | Err(mpsc::RecvError) => {}
                }
            }
            let snapshot = session_fixture(&["/dev/video0"], request.settings.video.mode);
            let outcome = validate_prepared(request.settings.clone(), &snapshot, &fixture_audio());
            (request, outcome)
        }

        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        RUNNER_START_TX.set(started_tx).unwrap();
        RUNNER_GATE_RX.set(std::sync::Mutex::new(gate_rx)).unwrap();

        let mut validator = CaptureValidator::with_runner(execute);
        // ApplyId 2 matches the gated runner branch above.
        let original = request(settings(
            fixture_identity(),
            mode(),
            AudioSelection::default(),
        ));
        let original = ValidationRequest {
            key: ValidationKey {
                apply: ApplyId::new(2).unwrap(),
                purpose: original.key.purpose,
            },
            ..original
        };
        validator.begin_validate(original.clone()).unwrap();
        started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("runner never signalled start");

        // Queue Stop while the request is still in flight: bounded(2) accepts it.
        validator.shutdown();
        assert!(
            validator.poll_validation().is_none(),
            "no result before completion"
        );
        gate_tx.send(()).expect("runner gate never closed"); // release the runner
        let result = wait_result(&mut validator);
        assert_eq!(result.request.key.apply, original.key.apply);
        assert!(result.result.is_ok());
        assert!(wait_shutdown(&mut validator));
    }

    #[test]
    fn panicking_runner_still_yields_terminal_original_request_result() {
        fn execute(request: ValidationRequest) -> WorkerOutcome {
            if request.key.apply.get() == 2 {
                panic!("fixture runner explosion");
            }
            let snapshot = session_fixture(&["/dev/video0"], request.settings.video.mode);
            let outcome = validate_prepared(request.settings.clone(), &snapshot, &fixture_audio());
            (request, outcome)
        }
        let mut validator = CaptureValidator::with_runner(execute);
        let gated = request(settings(
            fixture_identity(),
            mode(),
            AudioSelection::default(),
        ));
        let gated = ValidationRequest {
            key: ValidationKey {
                apply: ApplyId::new(2).unwrap(),
                purpose: gated.key.purpose,
            },
            ..gated
        };
        validator.begin_validate(gated.clone()).unwrap();
        let result = wait_result(&mut validator);
        // Original request identity surfaced with a typed terminal failure,
        // never a fake success; the slot is consumed and reusable.
        assert_eq!(result.request.key.apply, gated.key.apply);
        assert_eq!(result.request.settings, gated.settings);
        let failure = result.result.expect_err("panic must not fabricate success");
        assert!(matches!(
            failure.category,
            FailureCategory::Validation(ValidationLayer::Discovery)
        ));
        assert!(failure.diagnostic.contains("validation worker panicked"));
        let followup = request(settings(
            fixture_identity(),
            mode(),
            AudioSelection::default(),
        ));
        validator.begin_validate(followup.clone()).unwrap();
        assert!(wait_result(&mut validator).result.is_ok());
        validator.shutdown();
        assert!(wait_shutdown(&mut validator));
    }

    #[test]
    fn retired_worker_with_undrained_pending_disconnects_and_drains_once() {
        // Real accepted submission on a live worker; the result endpoint is
        // then swapped for a sender-less receiver and the worker retires with
        // the outcome stranded in the old channel. The pending request must
        // drain exactly once as a typed original-request failure, further
        // submits stay Disconnected, and shutdown completes.
        let mut validator = CaptureValidator::with_runner(fixture_runner());
        let original = request(settings(
            fixture_identity(),
            mode(),
            AudioSelection::default(),
        ));
        validator.begin_validate(original.clone()).unwrap();

        // Give the worker time to strand its outcome in the replaced channel.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let stranded = std::mem::replace(&mut validator.rx, {
            let (tx, rx) = mpsc::channel();
            drop(tx);
            rx
        });
        drop(stranded);

        validator.shutdown();
        assert!(wait_shutdown(&mut validator), "worker must actually retire");
        let result = validator
            .poll_validation()
            .expect("pending request must drain after worker retirement");
        assert_eq!(result.request.key.apply, original.key.apply);
        assert_eq!(result.request.settings, original.settings);
        let failure = result
            .result
            .expect_err("disconnect must not fabricate success");
        assert!(
            failure
                .diagnostic
                .contains("validation worker disconnected")
        );
        assert!(validator.poll_validation().is_none(), "drains exactly once");
        assert_eq!(
            validator.begin_validate(request(settings(
                fixture_identity(),
                mode(),
                AudioSelection::default()
            ))),
            Err(SubmitFailure::Disconnected),
            "future submits after genuine disconnect stay Disconnected"
        );
        assert!(validator.shutdown_complete());
    }

    #[test]
    fn result_channel_disconnect_drains_once_then_refuses_submits() {
        // Direct result-channel disconnect: outcome sender already gone, an
        // accepted request is pending, no worker thread exists. The pending
        // request must drain exactly once with a typed original-request
        // failure, and every later submit must stay Disconnected.
        let (tx, _command_rx) = mpsc::sync_channel::<Command>(2);
        let (outcome_tx, rx) = mpsc::channel();
        drop(outcome_tx);
        let pending = request(settings(
            fixture_identity(),
            mode(),
            AudioSelection::default(),
        ));
        let mut validator = CaptureValidator {
            tx,
            rx,
            handle: None,
            retired: Arc::new(AtomicBool::new(false)),
            pending: Some(pending.clone()),
            ready: None,
            shutdown_requested: false,
            stop_sent: false,
            worker_result_dead: false,
        };
        let result = validator
            .poll_validation()
            .expect("pending request must drain on disconnect");
        assert_eq!(result.request.key.apply, pending.key.apply);
        assert_eq!(result.request.settings, pending.settings);
        let failure = result
            .result
            .expect_err("disconnect must not fabricate success");
        assert!(
            failure
                .diagnostic
                .contains("validation worker disconnected")
        );
        assert!(validator.poll_validation().is_none(), "drains exactly once");
        assert_eq!(
            validator.begin_validate(request(settings(
                fixture_identity(),
                mode(),
                AudioSelection::default()
            ))),
            Err(SubmitFailure::Disconnected),
            "reuse after genuine result disconnect is refused"
        );
        validator.shutdown();
        assert!(validator.shutdown_complete());
    }

    #[test]
    fn range_descriptors_use_injected_exact_queries_only() {
        use crate::capture::linux::{fixture_exact_intervals, session_range_fixture};

        let mode = mode();
        // Two same-identity nodes with identical range descriptors: both prove
        // the tuple through the injected query, so the route is ambiguous.
        let snapshot = session_range_fixture(&["/dev/video0", "/dev/video1"], mode);
        let mut queries = 0usize;
        let prepared = validate_prepared_fixture(
            settings(fixture_identity(), mode, AudioSelection::default()),
            &snapshot,
            &[],
            |_device, _node, fourcc, size| {
                queries += 1;
                Ok(fixture_exact_intervals(fourcc, size, mode.rate))
            },
        );
        assert!(
            prepared.is_err(),
            "ambiguous eligible routes must fail closed"
        );
        assert_eq!(
            queries, 2,
            "each eligible route queried exactly once, no /dev I/O"
        );

        // A single range node prepares normally with the same injected proof.
        let snapshot = session_range_fixture(&["/dev/video0"], mode);
        let mut queries = 0usize;
        let prepared = validate_prepared_fixture(
            settings(fixture_identity(), mode, AudioSelection::default()),
            &snapshot,
            &[],
            |_device, _node, fourcc, size| {
                queries += 1;
                Ok(fixture_exact_intervals(fourcc, size, mode.rate))
            },
        )
        .unwrap();
        assert_eq!(queries, 1);
        assert_eq!(prepared.input().node(), Path::new("/dev/video0"));
    }
}
