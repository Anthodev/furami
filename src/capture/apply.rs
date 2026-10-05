//! Video-first fresh identity/tuple validation and lifetime recovery observation.
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
    app::ports::{DraftValidator, SubmitFailure, ValidationOutcome, ValidationResult},
    capture::{
        audio::AudioSource,
        input::{CaptureSelection, InputError, InputSpec, SelectionError},
        linux::{self, CaptureError, CaptureSnapshot, ValidatedCapture},
    },
    domain::{
        capture::{
            AudioSelection, AudioSourceIdentity, CaptureBufferType, RecoveryObservation,
            RecoveryWatchTarget, SelectedRouteAuthorization, VideoPresence, WatchStamp,
        },
        failure::{ApplyFailure, Cause, FailureCategory, Stage, ValidationLayer},
        state::{DraftSettings, ValidationKey, ValidationRequest},
    },
};

/// Fully proven video plan. Desired audio remains enabled even while its exact
/// source is absent; audio readiness requires the media owner's route proof.
#[derive(Debug)]
pub struct PreparedCapture {
    settings: DraftSettings,
    selection: CaptureSelection,
    input: InputSpec,
    audio: Option<AudioSourceIdentity>,
    stamp: WatchStamp,
    selected_route: Option<SelectedRouteAuthorization>,
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

    pub fn stamp(&self) -> WatchStamp {
        self.stamp
    }

    pub fn selected_route(&self) -> Option<&SelectedRouteAuthorization> {
        self.selected_route.as_ref()
    }

    pub fn into_parts(
        self,
    ) -> (
        DraftSettings,
        CaptureSelection,
        InputSpec,
        Option<AudioSourceIdentity>,
        WatchStamp,
        Option<SelectedRouteAuthorization>,
    ) {
        (
            self.settings,
            self.selection,
            self.input,
            self.audio,
            self.stamp,
            self.selected_route,
        )
    }
}

/// Fresh video tuple proof associated with the completed subscribed observation.
/// Audio is desired configuration, not permission to reject a healthy video.
pub fn validate_prepared(
    settings: DraftSettings,
    snapshot: &CaptureSnapshot,
    audio_catalog: &[AudioSource],
    stamp: WatchStamp,
) -> Result<PreparedCapture, ApplyFailure> {
    validate_prepared_with(
        settings,
        snapshot,
        audio_catalog,
        stamp,
        None,
        linux::query_intervals,
    )
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
    validate_prepared_with(
        settings,
        snapshot,
        audio_catalog,
        fixture_stamp(),
        None,
        query,
    )
}

/// Preserve an explicit selection through the owner's second fresh validation.
/// None retains strict automatic identity resolution. Some checks complete
/// identity and both physical syspaths, then the same full saved tuple.
pub(crate) fn revalidate_authorized_input(
    settings: &DraftSettings,
    selected_route: Option<&SelectedRouteAuthorization>,
    snapshot: &CaptureSnapshot,
) -> Result<InputSpec, ApplyFailure> {
    revalidate_authorized_with(
        settings,
        selected_route,
        snapshot,
        linux::query_intervals,
        linux::revalidate_route,
    )
}

fn revalidate_authorized_with(
    settings: &DraftSettings,
    selected_route: Option<&SelectedRouteAuthorization>,
    snapshot: &CaptureSnapshot,
    query: impl FnMut(
        &linux::CaptureDevice,
        &linux::CaptureNode,
        crate::domain::capture::CapturedFourCc,
        crate::domain::capture::FrameSize,
    ) -> Result<linux::ExactSizeIntervals, CaptureError>,
    check_route: impl FnOnce(
        &crate::domain::capture::DeviceIdentity,
        &linux::ValidatedRoute<'_>,
    ) -> Result<(), CaptureError>,
) -> Result<InputSpec, ApplyFailure> {
    if let Some(selected) = selected_route
        && settings.video.identity != selected.identity
    {
        return Err(capture_failure(
            settings,
            "match the prepared selected identity",
            &CaptureError::Identity(crate::domain::capture::IdentityError::NotFound),
        ));
    }
    let validated = validate_video_with(settings, snapshot, selected_route, query)?;
    let node = unique_single_planar_node(settings, &validated)?;
    if let Some(selected) = selected_route {
        ensure_selected_node(settings, &validated, &node, selected)?;
    }
    let route = validated
        .routes()
        .iter()
        .find(|route| {
            route.buffer_type() == CaptureBufferType::SinglePlanar && route.node().devnode() == node
        })
        .expect("unique_single_planar_node proved a route");
    check_route(validated.identity(), route).map_err(|error| {
        capture_failure(settings, "fresh-check authorized capture route", &error)
    })?;
    InputSpec::from_validated(&validated, Some(&node))
        .map_err(|error| input_failure(settings, "construct fresh authorized input", &error))
}

fn validate_video_with<'a>(
    settings: &DraftSettings,
    snapshot: &'a CaptureSnapshot,
    selected_route: Option<&SelectedRouteAuthorization>,
    query: impl FnMut(
        &linux::CaptureDevice,
        &linux::CaptureNode,
        crate::domain::capture::CapturedFourCc,
        crate::domain::capture::FrameSize,
    ) -> Result<linux::ExactSizeIntervals, CaptureError>,
) -> Result<ValidatedCapture<'a>, ApplyFailure> {
    let validated = match selected_route {
        None => linux::validate_with_injected_query(snapshot, &settings.video, query),
        Some(selected) => {
            linux::validate_authorized_with_query(snapshot, &settings.video, selected, query)
        }
    };
    validated.map_err(|error| capture_failure(settings, "validate requested tuple", &error))
}

fn ensure_selected_node(
    settings: &DraftSettings,
    validated: &ValidatedCapture<'_>,
    node: &std::path::Path,
    selected: &SelectedRouteAuthorization,
) -> Result<(), ApplyFailure> {
    if validated.routes().iter().any(|route| {
        route.node().devnode() == node
            && route.node().syspath() == selected.node_syspath
            && route.node().usb_syspath() == selected.usb_syspath
    }) {
        Ok(())
    } else {
        Err(capture_failure(
            settings,
            "revalidate explicitly selected physical route",
            &CaptureError::StaleSnapshot {
                path: selected.node_syspath.clone(),
            },
        ))
    }
}

fn validate_prepared_with(
    mut settings: DraftSettings,
    snapshot: &CaptureSnapshot,
    _audio_catalog: &[AudioSource],
    stamp: WatchStamp,
    selected_route: Option<SelectedRouteAuthorization>,
    query: impl FnMut(
        &crate::capture::linux::CaptureDevice,
        &crate::capture::linux::CaptureNode,
        crate::domain::capture::CapturedFourCc,
        crate::domain::capture::FrameSize,
    ) -> Result<crate::capture::linux::ExactSizeIntervals, CaptureError>,
) -> Result<PreparedCapture, ApplyFailure> {
    let validated = validate_video_with(&settings, snapshot, selected_route.as_ref(), query)?;
    let node = unique_single_planar_node(&settings, &validated)?;
    if let Some(selected) = &selected_route {
        ensure_selected_node(&settings, &validated, &node, selected)?;
    }
    settings.video.identity = validated.identity().clone();
    let selection = CaptureSelection::from_snapshot(snapshot, node.as_path(), settings.video.mode)
        .map_err(|error| selection_failure(&settings, "resolve the selected route", &error))?;
    // Built from the same proven ValidatedCapture: one validation pass, no
    // second live interval query, identical route proof.
    let input = InputSpec::from_validated(&validated, Some(node.as_path()))
        .map_err(|error| input_failure(&settings, "construct the pinned capture input", &error))?;
    let audio = match &settings.audio {
        AudioSelection::Disabled { .. } => None,
        AudioSelection::Enabled { source } => Some(source.clone()),
    };
    Ok(PreparedCapture {
        settings,
        selection,
        input,
        audio,
        stamp,
        selected_route,
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

pub(super) fn capture_failure(
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

#[cfg(test)]
pub(crate) fn fixture_prepared(settings: DraftSettings) -> Result<PreparedCapture, ApplyFailure> {
    fixture_prepared_at(settings, fixture_stamp())
}

#[cfg(test)]
pub(crate) fn fixture_prepared_at(
    settings: DraftSettings,
    stamp: WatchStamp,
) -> Result<PreparedCapture, ApplyFailure> {
    let snapshot = crate::capture::linux::session_fixture(&["/dev/video0"], settings.video.mode);
    validate_prepared(settings, &snapshot, &[], stamp)
}

#[cfg(test)]
pub(super) fn fixture_authorized_prepared_at(
    settings: DraftSettings,
    snapshot: &CaptureSnapshot,
    stamp: WatchStamp,
    selected: SelectedRouteAuthorization,
) -> Result<PreparedCapture, ApplyFailure> {
    let rate = settings.video.mode.rate;
    validate_prepared_with(
        settings,
        snapshot,
        &[],
        stamp,
        Some(selected),
        |_device, _node, fourcc, size| Ok(linux::fixture_exact_intervals(fourcc, size, rate)),
    )
}

#[cfg(test)]
fn fixture_stamp() -> WatchStamp {
    WatchStamp {
        watch: crate::domain::capture::WatchId::new(1).unwrap(),
        epoch: crate::domain::capture::ObservationEpoch::new(1).unwrap(),
    }
}

type Command = Option<ValidationRequest>;

#[cfg(test)]
type WorkerOutcome = (ValidationRequest, Result<PreparedCapture, ApplyFailure>);
type CompletedValidation = (
    ValidationRequest,
    WatchStamp,
    ValidationOutcome<PreparedCapture>,
);

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
    rx: mpsc::Receiver<CompletedValidation>,
    handle: Option<JoinHandle<()>>,
    retired: Arc<AtomicBool>,
    observation: super::watch::ObservationWorker,
    pending: Option<ValidationRequest>,
    ready: Option<ValidationResult<PreparedCapture>>,
    shutdown_requested: bool,
    stop_sent: bool,
    /// Set once an accepted request was drained via channel disconnect:
    /// further submits are refused because no result can ever arrive.
    worker_result_dead: bool,
}

fn run_validation(
    request: ValidationRequest,
    observer: &super::watch::Observer,
) -> CompletedValidation {
    let (scan, selected_route) = match observer.fresh(&request) {
        Ok(fresh) => fresh,
        Err(error) => {
            let stamp = observer.stamp(request.watch);
            return (request, stamp, ValidationOutcome::Failed(error));
        }
    };
    let stamp = scan.observation.stamp;
    let outcome = match &scan.snapshot {
        Err(error) => ValidationOutcome::Failed(error.clone()),
        Ok(snapshot) => {
            if request.choice.is_none()
                && scan.target.video == request.settings.video
                && let VideoPresence::Ambiguous(candidates) = &scan.observation.video
            {
                let mut eligible = Vec::new();
                let mut failure = None;
                for candidate in candidates {
                    let Some(authorization) = scan
                        .candidates
                        .iter()
                        .find(|authorization| authorization.token == candidate.token)
                    else {
                        continue;
                    };
                    let proof = (|| {
                        let validated = validate_video_with(
                            &request.settings,
                            snapshot,
                            Some(authorization),
                            linux::query_intervals,
                        )?;
                        let node = unique_single_planar_node(&request.settings, &validated)?;
                        ensure_selected_node(&request.settings, &validated, &node, authorization)
                    })();
                    match proof {
                        Ok(()) => eligible.push(candidate.clone()),
                        Err(error) => {
                            failure.get_or_insert(error);
                        }
                    }
                }
                if eligible.is_empty() {
                    ValidationOutcome::Failed(failure.unwrap_or_else(||
                        ApplyFailure::new(FailureCategory::Validation(ValidationLayer::Input),
                            Stage::Prevalidation, Cause::RequestedModeRefused,
                            request.settings.clone(), "select an eligible physical capture candidate",
                            "no candidate has a unique supported single-planar route for the saved tuple")))
                } else {
                    ValidationOutcome::SelectionRequired(eligible)
                }
            } else {
                match validate_prepared_with(
                    request.settings.clone(),
                    snapshot,
                    &[],
                    stamp,
                    selected_route,
                    linux::query_intervals,
                ) {
                    Ok(prepared) => ValidationOutcome::Prepared(prepared),
                    Err(error) => ValidationOutcome::Failed(error),
                }
            }
        }
    };
    (request, stamp, outcome)
}

impl CaptureValidator {
    pub fn new() -> Self {
        let observation = super::watch::ObservationWorker::new();
        let observer = observation.observer.clone();
        Self::with_executor(observation, move |request| {
            run_validation(request, &observer)
        })
    }

    /// Deterministic test runner, never a second production validation path.
    #[cfg(test)]
    pub fn with_runner(execute: fn(ValidationRequest) -> WorkerOutcome) -> Self {
        Self::with_runner_and_audio_catalog(execute, Vec::new())
    }

    #[cfg(test)]
    pub fn with_runner_and_audio_catalog(
        execute: fn(ValidationRequest) -> WorkerOutcome,
        catalog: Vec<AudioSource>,
    ) -> Self {
        Self::with_executor(
            super::watch::ObservationWorker::fixture_with_audio_catalog(catalog),
            move |request| {
                let (request, result) = execute(request);
                let stamp = request.watch;
                let result = match result {
                    Ok(mut prepared) => {
                        prepared.stamp = stamp;
                        ValidationOutcome::Prepared(prepared)
                    }
                    Err(error) => ValidationOutcome::Failed(error),
                };
                (request, stamp, result)
            },
        )
    }

    fn with_executor(
        observation: super::watch::ObservationWorker,
        execute: impl Fn(ValidationRequest) -> CompletedValidation + Send + 'static,
    ) -> Self {
        // Bounded at 2: at most one Validate (slot guard) plus one Stop can
        // ever be queued, so Stop submission can never wedge behind Full.
        let (tx, command_rx) = mpsc::sync_channel::<Option<ValidationRequest>>(2);
        let (outcome_tx, rx) = mpsc::sync_channel(1);
        let retired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&retired);
        let spawned = std::thread::Builder::new()
            .name("furami-draft-validation".into())
            .spawn(move || {
                while let Ok(command) = command_rx.recv() {
                    match command {
                        Some(request) => {
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
                                let stamp = request.watch;
                                (request, stamp, ValidationOutcome::Failed(failure))
                            });
                            // Exact prevalidation-pass evidence lives with the
                            // capture adapter; runtime only consumes the port.
                            let payload = match &outcome.2 {
                                ValidationOutcome::Prepared(_) => serde_json::json!({
                                    "request": outcome.0, "stamp": outcome.1,
                                    "status": "Validated", "failure": null,
                                }),
                                ValidationOutcome::SelectionRequired(candidates) => serde_json::json!({
                                    "request": outcome.0, "stamp": outcome.1,
                                    "status": "SelectionRequired", "candidates": candidates,
                                }),
                                ValidationOutcome::Failed(failure) => serde_json::json!({
                                    "request": outcome.0, "stamp": outcome.1,
                                    "status": "Failed", "failure": failure,
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
                        None => break,
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
                observation,
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
                observation,
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

    fn prepared_settings(prepared: &Self::Prepared) -> &DraftSettings {
        prepared.settings()
    }

    fn prepared_stamp(prepared: &Self::Prepared) -> WatchStamp {
        prepared.stamp()
    }

    fn watch(&mut self, target: RecoveryWatchTarget) -> Result<(), SubmitFailure> {
        if self.shutdown_requested {
            return Err(SubmitFailure::Disconnected);
        }
        self.observation.observer.watch(target)
    }

    fn retire_selection(&mut self, token: crate::domain::capture::SelectionToken) {
        self.observation.observer.retire_choice(token);
    }

    fn poll_recovery(&mut self) -> Option<RecoveryObservation> {
        self.observation.observer.poll()
    }

    fn clear_watch(&mut self) {
        self.observation.observer.clear();
    }

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
            .try_send(Some(request.clone()))
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
                Ok((request, stamp, result)) => {
                    self.ready = Some(ValidationResult {
                        request,
                        stamp,
                        result,
                    });
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
                            stamp: self.observation.observer.stamp(pending.watch),
                            result: ValidationOutcome::Failed(worker_disconnected_failure(
                                &pending,
                                "validation worker disconnected before delivering its terminal result",
                            )),
                        });
                    }
                }
            }
        }
        let result = self.ready.take();
        if let Some(result) = &result
            && matches!(result.result, ValidationOutcome::Failed(_))
            && let Some(token) = result.request.choice
        {
            self.observation.observer.retire_choice(token);
        }
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
            if let Some(token) = pending.choice {
                self.observation.observer.retire_choice(token);
            }
        }
    }

    fn shutdown(&mut self) {
        self.shutdown_requested = true;
        self.observation.shutdown();
        // Level-triggered: at most one Stop is ever enqueued.
        // Level-triggered with retry: stop_sent is committed only once a Stop
        // is actually queued. Bounded(2) makes Full impossible for Stop here.
        if !self.stop_sent && self.tx.try_send(None).is_ok() {
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
        self.observation.joined()
    }
}

impl Drop for CaptureValidator {
    fn drop(&mut self) {
        self.shutdown();
        // Final ownership fallback; normal coordinator shutdown joins only
        // finished threads and remains nonblocking.
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
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
            playback: crate::domain::state::InitialPlayback::Live,
            watch: fixture_stamp(),
            choice: None,
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

    fn validate_prepared(
        settings: DraftSettings,
        snapshot: &CaptureSnapshot,
        catalog: &[AudioSource],
    ) -> Result<PreparedCapture, ApplyFailure> {
        super::validate_prepared(settings, snapshot, catalog, fixture_stamp())
    }

    trait OutcomeAssertions {
        fn is_ok(&self) -> bool;
        fn expect_err(self, detail: &str) -> ApplyFailure;
    }
    impl OutcomeAssertions for ValidationOutcome<PreparedCapture> {
        fn is_ok(&self) -> bool {
            matches!(self, ValidationOutcome::Prepared(_))
        }
        fn expect_err(self, detail: &str) -> ApplyFailure {
            match self {
                ValidationOutcome::Failed(error) => error,
                _ => panic!("{detail}"),
            }
        }
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
        // Missing enabled audio does not veto a fully verified video tuple.
        let prepared = validate_prepared(
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
        .unwrap();
        assert_eq!(prepared.audio(), Some(&source("missing", "gone").identity));
        assert!(prepared.settings().audio.enabled());
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
    fn changed_audio_properties_retain_exact_desire_without_substitution() {
        let mode = mode();
        let snapshot = session_fixture(&["/dev/video0"], mode);
        let prepared = validate_prepared(
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
        .unwrap();
        assert_eq!(
            prepared.audio(),
            Some(&source("fixture-source", "REPLACED-CARD").identity)
        );
        assert_ne!(prepared.audio(), Some(&fixture_audio()[0].identity));
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
        static RUNNER_GATE_RX: OnceLock<parking_lot::Mutex<mpsc::Receiver<()>>> = OnceLock::new();

        fn execute(request: ValidationRequest) -> WorkerOutcome {
            // Park only the gated validation (ApplyId 2) until released.
            if request.key.apply.get() == 2 {
                RUNNER_START_TX.get().unwrap().send(()).unwrap();
                // Gate stays closed until the test's explicit send; only a
                // dropped sender (test unwind/failure) releases the worker.
                match RUNNER_GATE_RX.get().unwrap().lock().recv() {
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
        RUNNER_GATE_RX
            .set(parking_lot::Mutex::new(gate_rx))
            .unwrap();

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
            !validator.shutdown_complete(),
            "the in-flight validator has not actually joined"
        );
        assert!(
            validator.poll_validation().is_none(),
            "no result before completion"
        );
        gate_tx.send(()).expect("runner gate never closed"); // release the runner
        let result = wait_result(&mut validator);
        assert_eq!(result.request.key.apply, original.key.apply);
        assert!(result.result.is_ok());
        assert!(wait_shutdown(&mut validator));
        assert!(
            validator.handle.is_none(),
            "validation join must consume its worker handle"
        );
        assert!(
            validator.observation.is_joined(),
            "observation join must also consume its handle"
        );
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
            observation: super::super::watch::ObservationWorker::fixture(),
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
        assert!(wait_shutdown(&mut validator));
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

    fn physical_identity(port: u8) -> DeviceIdentity {
        DeviceIdentity::new(
            0x32ed,
            0x3701,
            crate::domain::capture::UsbTopology::new(
                "pci-fixture".into(),
                vec![std::num::NonZeroU8::new(port).unwrap()],
            )
            .unwrap(),
            None,
        )
        .unwrap()
    }

    fn authorization(identity: DeviceIdentity) -> SelectedRouteAuthorization {
        SelectedRouteAuthorization {
            token: crate::domain::capture::SelectionToken {
                stamp: fixture_stamp(),
                candidate: crate::domain::capture::CandidateId::new(2).unwrap(),
            },
            identity,
            usb_syspath: "/sys/usb/2".into(),
            node_syspath: "/sys/usb/2/video1".into(),
        }
    }

    fn authorized_input(
        requested: &DraftSettings,
        selected: Option<&SelectedRouteAuthorization>,
        snapshot: &CaptureSnapshot,
    ) -> Result<InputSpec, ApplyFailure> {
        revalidate_authorized_with(
            requested,
            selected,
            snapshot,
            |_device, _node, fourcc, size| {
                Ok(linux::fixture_exact_intervals(fourcc, size, mode().rate))
            },
            |_identity, _route| Ok(()),
        )
    }

    #[test]
    fn explicit_choice_survives_second_fresh_validation_without_relaxing_automatic_resolution() {
        let saved = physical_identity(1);
        let chosen = physical_identity(2);
        let snapshot = linux::physical_fixture(&[
            (
                saved.clone(),
                "/dev/video0",
                "/sys/usb/1",
                "/sys/usb/1/video0",
                mode(),
            ),
            (
                chosen.clone(),
                "/dev/video1",
                "/sys/usb/2",
                "/sys/usb/2/video1",
                mode(),
            ),
        ]);
        let requested = settings(saved, mode(), AudioSelection::default());
        assert!(
            authorized_input(&requested, None, &snapshot).is_err(),
            "the automatic resolver remains fail-closed for serial-less duplicates"
        );
        let selected = authorization(chosen.clone());
        let prepared = validate_prepared_with(
            requested,
            &snapshot,
            &[],
            fixture_stamp(),
            Some(selected.clone()),
            |_device, _node, fourcc, size| {
                Ok(linux::fixture_exact_intervals(fourcc, size, mode().rate))
            },
        )
        .unwrap();
        assert_eq!(prepared.settings().video.identity, chosen);
        assert_eq!(prepared.settings().video.mode, mode());
        let second =
            authorized_input(prepared.settings(), prepared.selected_route(), &snapshot).unwrap();
        assert_eq!(second.node(), Path::new("/dev/video1"));
        assert_eq!(second.requested().identity, chosen);
        assert_eq!(second.requested().mode, mode());
        assert!(
            authorized_input(prepared.settings(), None, &snapshot).is_err(),
            "authorization is explicit, not persisted as an automatic resolver shortcut"
        );
    }

    #[test]
    fn authorized_second_validation_rejects_replaced_identity_or_either_changed_syspath() {
        let chosen = physical_identity(2);
        let selected = authorization(chosen.clone());
        let requested = settings(chosen.clone(), mode(), AudioSelection::default());
        for snapshot in [
            linux::physical_fixture(&[(
                physical_identity(3),
                "/dev/video1",
                "/sys/usb/2",
                "/sys/usb/2/video1",
                mode(),
            )]),
            linux::physical_fixture(&[(
                chosen.clone(),
                "/dev/video1",
                "/sys/usb/new",
                "/sys/usb/2/video1",
                mode(),
            )]),
            linux::physical_fixture(&[(
                chosen.clone(),
                "/dev/video1",
                "/sys/usb/2",
                "/sys/usb/2/replaced",
                mode(),
            )]),
            linux::empty_snapshot(),
        ] {
            assert!(authorized_input(&requested, Some(&selected), &snapshot).is_err());
        }
        // Renumbering a node name alone does not change the one-open locator.
        let renumbered = linux::physical_fixture(&[(
            chosen,
            "/dev/video9",
            "/sys/usb/2",
            "/sys/usb/2/video1",
            mode(),
        )]);
        assert_eq!(
            authorized_input(&requested, Some(&selected), &renumbered)
                .unwrap()
                .node(),
            Path::new("/dev/video9")
        );
    }

    #[test]
    fn every_authorized_reopen_revalidates_fourcc_size_and_exact_rational_rate() {
        let chosen = physical_identity(2);
        let selected = authorization(chosen.clone());
        let requested = settings(chosen.clone(), mode(), AudioSelection::default());
        let mut changed = mode();
        changed.captured_fourcc = CapturedFourCc::from_bytes(*b"YUYV");
        let mut resized = mode();
        resized.size = FrameSize::new(1920, 1080).unwrap();
        let mut rerated = mode();
        rerated.rate = FrameRate::new(60000, 1001).unwrap();
        for advertised in [changed, resized, rerated] {
            let snapshot = linux::physical_fixture(&[(
                chosen.clone(),
                "/dev/video1",
                "/sys/usb/2",
                "/sys/usb/2/video1",
                advertised,
            )]);
            let failure = authorized_input(&requested, Some(&selected), &snapshot).unwrap_err();
            assert!(matches!(
                failure.category,
                FailureCategory::Validation(ValidationLayer::Mode)
            ));
            assert_eq!(*failure.requested, requested);
        }
    }

    #[test]
    fn second_validation_preserves_live_capability_failure_instead_of_using_cached_route_proof() {
        let requested = settings(fixture_identity(), mode(), AudioSelection::default());
        let snapshot = session_fixture(&["/dev/video0"], mode());
        let failure = revalidate_authorized_with(
            &requested,
            None,
            &snapshot,
            |_device, _node, fourcc, size| {
                Ok(linux::fixture_exact_intervals(fourcc, size, mode().rate))
            },
            |_identity, route| {
                Err(CaptureError::StaleSnapshot {
                    path: route.node().devnode().to_owned(),
                })
            },
        )
        .unwrap_err();
        assert_eq!(failure.stage, Stage::Prevalidation);
        assert!(failure.diagnostic.contains("no longer matches"));
    }
}
