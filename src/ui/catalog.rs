//! One bounded latest-request worker; Linux/Pulse discovery and exact ioctls never run on Qt.
use super::projection::*;
use crate::{
    capture::{audio, linux},
    domain::capture::{
        AudioSelection, CaptureModeRequest, CapturedFourCc, Descriptor, DeviceIdentity,
        FrameInterval, FrameIntervalKind, FrameIntervals, FrameRate, FrameSize, FrameSizeKind,
        InputFormatPolicy, VideoRequest,
    },
};
use parking_lot::{Condvar, Mutex};
use std::{
    path::PathBuf,
    sync::Arc,
    thread::{self, JoinHandle},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Physical {
    pub identity: DeviceIdentity,
    pub node: PathBuf,
    pub usb: PathBuf,
}
// Row and query views share the immutable observation instead of copying V4L2 descriptors.
#[derive(Clone, Debug)]
pub(super) struct DeviceView {
    snapshot: Arc<linux::CaptureSnapshot>,
    index: usize,
}
impl std::ops::Deref for DeviceView {
    type Target = linux::CaptureDevice;
    fn deref(&self) -> &Self::Target {
        &self.snapshot.devices()[self.index]
    }
}
#[derive(Clone, Debug)]
pub(super) struct NodeView {
    device: DeviceView,
    index: usize,
}
impl std::ops::Deref for NodeView {
    type Target = linux::CaptureNode;
    fn deref(&self) -> &Self::Target {
        &self.device.nodes()[self.index]
    }
}
#[derive(Clone, Debug)]
pub(super) struct Source {
    pub device: DeviceView,
    pub node: NodeView,
}
impl Source {
    fn snapshot_rows(snapshot: &Arc<linux::CaptureSnapshot>) -> Vec<Self> {
        let mut rows = Vec::with_capacity(
            snapshot
                .devices()
                .iter()
                .map(|device| device.nodes().len())
                .sum(),
        );
        for (device_index, device) in snapshot.devices().iter().enumerate() {
            for node_index in 0..device.nodes().len() {
                let device = DeviceView {
                    snapshot: snapshot.clone(),
                    index: device_index,
                };
                let node = NodeView {
                    device: device.clone(),
                    index: node_index,
                };
                rows.push(Self { device, node });
            }
        }
        rows
    }
    pub fn physical(&self) -> Physical {
        Physical {
            identity: self.device.identity().clone(),
            node: self.node.syspath().into(),
            usb: self.node.usb_syspath().into(),
        }
    }
    fn matches(&self, physical: &Physical) -> bool {
        self.device.identity() == &physical.identity
            && self.node.syspath() == physical.node.as_path()
            && self.node.usb_syspath() == physical.usb.as_path()
    }
    fn supports_format(&self, fourcc: CapturedFourCc) -> bool {
        self.node.capabilities().formats().iter().any(|format| {
            format.captured_fourcc == fourcc
                && format.buffer_type == crate::domain::capture::CaptureBufferType::SinglePlanar
        })
    }
    pub fn supports_input(&self) -> bool {
        self.node.capabilities().formats().iter().any(|format| {
            format.buffer_type == crate::domain::capture::CaptureBufferType::SinglePlanar
                && crate::capture::input::supports_fourcc(format.captured_fourcc)
        })
    }
    fn same_observation(&self, other: &Self) -> bool {
        self.device.identity() == other.device.identity()
            && self.node.syspath() == other.node.syspath()
            && self.node.usb_syspath() == other.node.usb_syspath()
            && self.node.devnode() == other.node.devnode()
            && self.node.card_name() == other.node.card_name()
            && self.node.driver() == other.node.driver()
            && self.node.bus_info() == other.node.bus_info()
            && self.node.capabilities() == other.node.capabilities()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Context {
    revision: u64,
    physical: Physical,
    policy: InputFormatPolicy,
    size: FrameSize,
}
struct Query {
    serial: u64,
    context: Context,
    source: Source,
    formats: Vec<CapturedFourCc>,
}
struct Intervals {
    serial: u64,
    context: Context,
    values: Vec<(CapturedFourCc, Result<Descriptor<FrameIntervals>, String>)>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ModeIntent {
    Strict,
    RestoreProfile,
}
struct ModeQuery {
    serial: u64,
    revision: u64,
    source: Source,
    policy: InputFormatPolicy,
    prior: Option<CaptureModeRequest>,
    intent: ModeIntent,
}
pub(super) struct ModeReply {
    pub serial: u64,
    pub revision: u64,
    pub physical: Physical,
    pub result: Result<CaptureModeRequest, String>,
}
enum Request {
    Intervals(Query),
    Mode(ModeQuery),
}
pub(super) enum ModeResolution {
    Ready(CaptureModeRequest),
    Pending(u64),
}
struct Discovery {
    capture: Result<linux::CaptureSnapshot, String>,
    audio: Result<Vec<audio::CatalogSource>, String>,
}
#[derive(Default)]
struct Slots {
    stop: bool,
    refresh: bool,
    query: Option<Request>,
    discovery: Option<Discovery>,
    intervals: Option<Intervals>,
    mode: Option<ModeReply>,
}
struct Shared {
    slots: Mutex<Slots>,
    ready: Condvar,
}
pub(super) struct Worker {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn start() -> Result<Self, String> {
        let shared = Arc::new(Shared {
            slots: Mutex::new(Slots::default()),
            ready: Condvar::new(),
        });
        let worker = shared.clone();
        let handle = thread::Builder::new()
            .name("ui-capture-catalog".into())
            .spawn(move || {
                loop {
                    let (refresh, query) = {
                        let mut slots = worker.slots.lock();
                        while !slots.stop && !slots.refresh && slots.query.is_none() {
                            worker.ready.wait(&mut slots);
                        }
                        if slots.stop {
                            return;
                        }
                        let refresh = std::mem::take(&mut slots.refresh);
                        (refresh, if refresh { None } else { slots.query.take() })
                    };
                    if refresh {
                        let capture = match linux::discover() {
                            Ok(snapshot) => Ok(snapshot),
                            Err(
                                linux::CaptureError::NoVideoNodes
                                | linux::CaptureError::NoCaptureNodes,
                            ) => Ok(linux::empty_snapshot()),
                            Err(error) => Err(error.to_string()),
                        };
                        let audio =
                            audio::discover_for_catalog().map_err(|error| error.to_string());
                        let mut slots = worker.slots.lock();
                        if slots.stop {
                            return;
                        }
                        slots.discovery = Some(Discovery { capture, audio });
                    } else if let Some(Request::Mode(query)) = query {
                        let result = resolve_mode_with_intent(
                            query.source.node.capabilities(),
                            query.policy,
                            query.prior,
                            query.intent,
                            |fourcc, size| {
                                linux::query_intervals(
                                    &query.source.device,
                                    &query.source.node,
                                    fourcc,
                                    size,
                                )
                                .map(|value| Some(value.intervals))
                                .map_err(|error| error.to_string())
                            },
                        )
                        .and_then(|mode| mode.ok_or_else(|| "Exact intervals unavailable.".into()));
                        let mut slots = worker.slots.lock();
                        if slots.stop {
                            return;
                        }
                        slots.mode = Some(ModeReply {
                            serial: query.serial,
                            revision: query.revision,
                            physical: query.source.physical(),
                            result,
                        });
                    } else if let Some(Request::Intervals(query)) = query {
                        let values = query
                            .formats
                            .into_iter()
                            .map(|fourcc| {
                                let result = linux::query_intervals(
                                    &query.source.device,
                                    &query.source.node,
                                    fourcc,
                                    query.context.size,
                                )
                                .map(|value| value.intervals)
                                .map_err(|error| error.to_string());
                                (fourcc, result)
                            })
                            .collect();
                        let mut slots = worker.slots.lock();
                        if slots.stop {
                            return;
                        }
                        slots.intervals = Some(Intervals {
                            serial: query.serial,
                            context: query.context,
                            values,
                        });
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }
    fn refresh(&self) {
        let mut slots = self.shared.slots.lock();
        if !slots.stop {
            slots.refresh = true;
            self.shared.ready.notify_one();
        }
    }
    fn query(&self, query: Query) {
        let mut slots = self.shared.slots.lock();
        if !slots.stop {
            slots.query = Some(Request::Intervals(query));
            self.shared.ready.notify_one();
        }
    }
    fn resolve_mode(&self, query: ModeQuery) -> Result<(), String> {
        let mut slots = self.shared.slots.lock();
        if slots.stop {
            return Err("The catalog is closing.".into());
        }
        slots.query = Some(Request::Mode(query));
        self.shared.ready.notify_one();
        Ok(())
    }
    fn take(&self) -> (Option<Discovery>, Option<Intervals>, Option<ModeReply>) {
        let mut slots = self.shared.slots.lock();
        (
            slots.discovery.take(),
            slots.intervals.take(),
            slots.mode.take(),
        )
    }
    pub fn stop(&self) {
        let mut slots = self.shared.slots.lock();
        slots.stop = true;
        slots.query = None;
        self.shared.ready.notify_one();
    }
    pub fn try_join(&mut self) -> Option<Result<(), String>> {
        if !self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            return None;
        }
        Some(
            self.handle
                .take()
                .expect("finished worker")
                .join()
                .map_err(|_| "capture catalog worker panicked".into()),
        )
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(super) struct Model {
    pub revision: u64,
    pub sources: Vec<Source>,
    pub audio: Vec<audio::AudioSource>,
    audio_usb: Vec<Result<Option<audio::UsbAssociation>, String>>,
    mode_reply: Option<ModeReply>,
    mode_pending: Option<u64>,
    pub snapshot: Option<Arc<linux::CaptureSnapshot>>,
    pub capture_error: String,
    pub audio_error: String,
    pub fresh: bool,
    pub audio_fresh: bool,
    pub refreshing: bool,
    context: Option<Context>,
    intervals: Option<Intervals>,
    query_pending: bool,
    query_sequence: u64,
    interval_refresh: bool,
    selection_context: Option<(Option<Physical>, InputFormatPolicy, Option<FrameSize>)>,
    pub projection: Catalog,
    sizes: Vec<FrameSize>,
    rates: Vec<FrameRate>,
    formats: Vec<CapturedFourCc>,
    pub worker: Option<Worker>,
    pub worker_error: String,
}
impl Default for Model {
    fn default() -> Self {
        Self {
            revision: 1,
            sources: Vec::new(),
            audio: Vec::new(),
            audio_usb: Vec::new(),
            mode_reply: None,
            mode_pending: None,
            snapshot: None,
            capture_error: String::new(),
            audio_error: String::new(),
            fresh: false,
            audio_fresh: false,
            refreshing: false,
            context: None,
            selection_context: None,
            intervals: None,
            query_pending: false,
            query_sequence: 0,
            interval_refresh: false,
            projection: Catalog::default(),
            sizes: Vec::new(),
            rates: Vec::new(),
            formats: Vec::new(),
            worker: None,
            worker_error: String::new(),
        }
    }
}
impl Model {
    pub fn start(&mut self) {
        if self.worker.is_none() && self.worker_error.is_empty() {
            match Worker::start() {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => self.worker_error = error,
            }
        }
        self.refresh();
    }
    pub fn refresh(&mut self) {
        if let Some(worker) = &self.worker
            && !self.refreshing
        {
            self.refreshing = true;
            worker.refresh();
        }
    }
    fn bump(&mut self) -> Result<(), String> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or("capture catalog revision exhausted")?;
        self.context = None;
        self.intervals = None;
        self.query_pending = false;
        self.interval_refresh = false;
        Ok(())
    }
    fn refresh_exact_context(&mut self) {
        self.interval_refresh = self.context.as_ref().is_some_and(|context| {
            self.source(&context.physical).is_some_and(|source| {
                source
                    .node
                    .capabilities()
                    .fourcc_capabilities()
                    .iter()
                    .any(|format| {
                        eligible_format(context.policy, format.captured_fourcc)
                            && source.supports_format(format.captured_fourcc)
                            && crate::capture::input::supports_fourcc(format.captured_fourcc)
                            && format.sizes.as_option().is_some_and(|sizes| {
                                !matches!(sizes.kind(), FrameSizeKind::Discrete(_))
                                    && sizes.contains(context.size)
                            })
                    })
            })
        });
        if self.interval_refresh {
            self.query_pending = false;
        }
    }
    fn accept_intervals(&mut self, mut intervals: Intervals) -> bool {
        if !self.query_pending
            || intervals.serial != self.query_sequence
            || self.context.as_ref() != Some(&intervals.context)
        {
            return false;
        }
        if self
            .intervals
            .as_ref()
            .is_none_or(|previous| previous.values != intervals.values)
        {
            if let Err(error) = self.bump() {
                self.capture_error = error;
                self.fresh = false;
                return true;
            }
            intervals.context.revision = self.revision;
            self.context = Some(intervals.context.clone());
        }
        self.intervals = Some(intervals);
        self.query_pending = false;
        self.interval_refresh = false;
        true
    }
    pub fn poll(&mut self) -> bool {
        let Some(worker) = &self.worker else {
            return false;
        };
        let (discovery, intervals, mode) = worker.take();
        if let Some(mode) = mode
            && self.mode_pending == Some(mode.serial)
        {
            self.mode_reply = Some(mode);
        }
        let mut changed = false;
        if let Some(result) = discovery {
            self.refreshing = false;
            changed = true;
            match result.capture {
                Ok(snapshot) => {
                    let snapshot = Arc::new(snapshot);
                    let sources = Source::snapshot_rows(&snapshot);
                    let different = self.sources.len() != sources.len()
                        || self
                            .sources
                            .iter()
                            .zip(&sources)
                            .any(|(a, b)| !a.same_observation(b))
                        || !self.fresh;
                    if different && let Err(error) = self.bump() {
                        self.capture_error = error;
                        self.fresh = false;
                        return true;
                    }
                    self.sources = sources;
                    self.snapshot = Some(snapshot);
                    self.capture_error.clear();
                    self.fresh = true;
                    if !different {
                        self.refresh_exact_context();
                    }
                }
                Err(error) => {
                    if self.fresh {
                        let _ = self.bump();
                    }
                    self.capture_error = error;
                    self.fresh = false;
                }
            }
            match result.audio {
                Ok(observations) => {
                    let different = observations.len() != self.audio.len()
                        || observations
                            .iter()
                            .zip(&self.audio)
                            .any(|(a, b)| a.source != *b)
                        || observations
                            .iter()
                            .zip(&self.audio_usb)
                            .any(|(a, b)| a.usb != *b)
                        || observations.len() != self.audio_usb.len()
                        || !self.audio_fresh;
                    if different && let Err(error) = self.bump() {
                        self.audio_error = error;
                        self.audio_fresh = false;
                        return true;
                    }
                    (self.audio, self.audio_usb) = observations
                        .into_iter()
                        .map(|row| (row.source, row.usb))
                        .unzip();
                    self.audio_error.clear();
                    self.audio_fresh = true;
                }
                Err(error) => {
                    if self.audio_fresh {
                        let _ = self.bump();
                    }
                    self.audio_error = error;
                    self.audio_fresh = false;
                }
            }
        }
        if let Some(intervals) = intervals {
            changed |= self.accept_intervals(intervals);
        }
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.handle.as_ref().is_some_and(JoinHandle::is_finished))
        {
            let stopped = self
                .worker
                .as_ref()
                .expect("finished worker")
                .shared
                .slots
                .lock()
                .stop;
            let result = self
                .worker
                .as_mut()
                .expect("finished worker")
                .try_join()
                .expect("finished worker");
            self.worker = None;
            if !stopped || result.is_err() {
                self.worker_error = result
                    .err()
                    .unwrap_or_else(|| "capture catalog worker ended unexpectedly".into());
                self.fresh = false;
                self.audio_fresh = false;
                self.refreshing = false;
                self.query_pending = false;
                let _ = self.bump();
                changed = true;
            }
        }
        changed
    }
    pub fn source(&self, physical: &Physical) -> Option<&Source> {
        self.sources.iter().find(|source| source.matches(physical))
    }
    pub fn source_for_video(&self, video: &VideoRequest) -> Option<&Source> {
        let mut matches = self.sources.iter().filter(|source| {
            source.device.identity() == &video.identity
                && video
                    .selected_node
                    .as_deref()
                    .is_none_or(|path| source.node.devnode() == path)
        });
        let source = matches.next()?;
        if matches.next().is_some() {
            None
        } else {
            Some(source)
        }
    }
    pub fn source_key(&self, physical: &Physical) -> String {
        self.sources
            .iter()
            .position(|source| source.matches(physical))
            .map(|index| format!("capture:{}:{index}", self.revision))
            .unwrap_or_default()
    }
    pub fn format_key(&self, fourcc: CapturedFourCc) -> String {
        self.formats
            .iter()
            .position(|value| *value == fourcc)
            .map(|index| format!("format:{}:{index}", self.revision))
            .unwrap_or_default()
    }
    pub fn input_key(&self, selection: &AudioSelection) -> String {
        let Some(identity) = selection.source() else {
            return String::new();
        };
        let mut matches = self
            .audio
            .iter()
            .enumerate()
            .filter(|(_, source)| identity.compatible_with(&source.identity));
        let Some((index, _)) = matches.next() else {
            return String::new();
        };
        if matches.next().is_some() {
            String::new()
        } else {
            format!("input:{}:{index}", self.revision)
        }
    }
    pub fn resolve_source(&self, revision: &str, key: &str) -> Result<&Source, String> {
        self.check(revision, self.fresh)?;
        let index = row_index(key, "capture", self.revision)?;
        self.sources
            .get(index)
            .ok_or_else(|| "capture row is stale".into())
    }
    pub fn resolve_input(&self, revision: &str, key: &str) -> Result<&audio::AudioSource, String> {
        self.check(revision, self.audio_fresh)?;
        let index = row_index(key, "input", self.revision)?;
        self.audio
            .get(index)
            .ok_or_else(|| "audio row is stale".into())
    }
    pub fn check(&self, revision: &str, fresh: bool) -> Result<(), String> {
        if decimal(revision) != Some(self.revision) || !fresh {
            Err("catalog is stale or unavailable; refresh it".into())
        } else {
            Ok(())
        }
    }
    pub fn resolve_format(&self, revision: &str, key: &str) -> Result<InputFormatPolicy, String> {
        self.check(revision, self.fresh)?;
        if key == "auto" {
            return Ok(InputFormatPolicy::Auto);
        }
        let index = row_index(key, "format", self.revision)?;
        self.formats
            .get(index)
            .copied()
            .map(|fourcc| InputFormatPolicy::Manual { fourcc })
            .ok_or_else(|| "format row is stale".into())
    }
    pub fn resolve_size(&self, revision: &str, key: &str) -> Result<FrameSize, String> {
        self.check(revision, self.fresh)?;
        self.projection
            .size_rows
            .iter()
            .position(|row| row.key == key)
            .and_then(|index| self.sizes.get(index))
            .copied()
            .ok_or_else(|| "size row is stale".into())
    }
    pub fn resolve_rate(&self, revision: &str, key: &str) -> Result<FrameRate, String> {
        self.check(revision, self.fresh)?;
        self.projection
            .rate_rows
            .iter()
            .position(|row| row.key == key && row.eligible)
            .and_then(|index| self.rates.get(index))
            .copied()
            .ok_or_else(|| "rate row is stale or awaiting exact intervals".into())
    }
    fn project_modes(&mut self, projection: &mut Catalog, size: Option<FrameSize>) {
        let context_key = size
            .map(|size| format!("{}x{}", size.width(), size.height()))
            .unwrap_or_else(|| "unset".into());
        self.sizes.sort_unstable_by(|a, b| size_cmp(*b, *a));
        self.rates.sort_unstable_by(|a, b| rate_cmp(*b, *a));
        for (index, size) in self.sizes.iter().enumerate() {
            projection.size_rows.push(SizeRow {
                key: format!("size:{}:{context_key}:{index}", self.revision),
                label: format!("{} × {}", size.width(), size.height()),
                width: size.width(),
                height: size.height(),
                eligible: self.fresh,
                reason: if self.fresh {
                    String::new()
                } else {
                    "Catalog unavailable".into()
                },
            });
        }
        for (index, rate) in self.rates.iter().enumerate() {
            let exact = format!("{}/{}", rate.numerator(), rate.denominator());
            let eligible = self.fresh && !self.query_pending && !self.interval_refresh;
            projection.rate_rows.push(RateRow {
                key: format!("rate:{}:{context_key}:{index}", self.revision),
                label: exact.clone(),
                numerator: rate.numerator(),
                denominator: rate.denominator(),
                exact,
                eligible,
                reason: if !self.fresh {
                    "Catalog unavailable".into()
                } else if !eligible {
                    "Reading exact intervals".into()
                } else {
                    String::new()
                },
            });
        }
    }
    pub fn resolve_source_mode(
        &mut self,
        physical: &Physical,
        policy: InputFormatPolicy,
        prior: Option<CaptureModeRequest>,
        intent: ModeIntent,
    ) -> Result<ModeResolution, String> {
        if self.mode_pending.is_some() {
            return Err("A source choice is already resolving.".into());
        }
        let source = self
            .source(physical)
            .filter(|_| self.fresh)
            .ok_or("The chosen physical node is no longer available.")?;
        if let Some(mode) =
            resolve_mode_with_intent(source.node.capabilities(), policy, prior, intent, |_, _| {
                Ok(None)
            })?
        {
            return Ok(ModeResolution::Ready(mode));
        }
        let worker = self
            .worker
            .as_ref()
            .ok_or("The catalog cannot read exact intervals.")?;
        let serial = self
            .query_sequence
            .checked_add(1)
            .ok_or("Request sequence exhausted.")?;
        worker.resolve_mode(ModeQuery {
            serial,
            revision: self.revision,
            source: source.clone(),
            policy,
            prior,
            intent,
        })?;
        self.query_sequence = serial;
        self.mode_pending = Some(serial);
        Ok(ModeResolution::Pending(serial))
    }
    pub fn take_mode_reply(&mut self) -> Option<ModeReply> {
        let reply = self.mode_reply.take()?;
        self.mode_pending = None;
        Some(reply)
    }
    pub fn cancel_mode(&mut self) {
        self.mode_pending = None;
        self.mode_reply = None;
    }
    pub fn associated_audio(&self, physical: &Physical) -> (AudioSelection, String) {
        if !self.audio_fresh {
            return (
                AudioSelection::default(),
                format!("Audio association unavailable: {}", self.audio_error),
            );
        }
        if self.audio_usb.len() != self.audio.len() {
            return (
                AudioSelection::default(),
                "USB audio association unavailable: physical evidence missing from the catalog."
                    .into(),
            );
        }
        if let Some(error) = self
            .audio_usb
            .iter()
            .find_map(|result| result.as_ref().err())
        {
            return (
                AudioSelection::default(),
                format!("USB audio association unavailable: {error} Manual choice available."),
            );
        }
        let mut matches = self.audio.iter().zip(&self.audio_usb).filter(|(_, usb)| {
            usb.as_ref().is_ok_and(|usb| {
                usb.as_ref().is_some_and(|usb| {
                    usb.syspath == physical.usb && usb.identity == physical.identity
                })
            })
        });
        match (matches.next(), matches.next()) {
            (Some((source, _)), None) => (
                AudioSelection::Enabled {
                    source: source.identity.clone(),
                },
                String::new(),
            ),
            (Some(_), Some(_)) => (
                AudioSelection::default(),
                "Multiple audio inputs on this USB card; choose an explicit input.".into(),
            ),
            _ => (
                AudioSelection::default(),
                "No audio input for this USB card in the catalog.".into(),
            ),
        }
    }
    pub fn rebuild(
        &mut self,
        physical: Option<&Physical>,
        policy: InputFormatPolicy,
        size: Option<FrameSize>,
        rate: Option<FrameRate>,
        audio: &AudioSelection,
    ) {
        let selection_context = (physical.cloned(), policy, size);
        if self
            .selection_context
            .as_ref()
            .is_some_and(|previous| previous != &selection_context)
            && let Err(error) = self.bump()
        {
            self.capture_error = error;
            self.fresh = false;
        }
        self.selection_context = Some(selection_context);
        let source = physical.and_then(|physical| self.source(physical)).cloned();
        let context = source.as_ref().and_then(|source| {
            size.map(|size| Context {
                revision: self.revision,
                physical: source.physical(),
                policy,
                size,
            })
        });
        if self.context != context {
            self.context = context.clone();
            self.intervals = None;
            self.query_pending = false;
            self.interval_refresh = false;
        }
        let mut projection = Catalog {
            revision: self.revision.to_string(),
            busy: self.refreshing,
            error: [
                self.worker_error.as_str(),
                self.capture_error.as_str(),
                self.audio_error.as_str(),
            ]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
            selected_source_key: physical
                .map(|physical| self.source_key(physical))
                .unwrap_or_default(),
            selected_audio_input_key: self.input_key(audio),
            capability_status: "unavailable".into(),
            capability_reason: "Select an explicit source.".into(),
            ..Catalog::default()
        };
        for (index, source) in self.sources.iter().enumerate() {
            let identity = source.device.identity();
            projection.source_rows.push(SourceRow { key: format!("capture:{}:{index}", self.revision), label: source.node.card_name().into(), detail: format!("{} · {} · {}", source.node.driver(), topology(identity), source.node.devnode().display()), eligible: self.fresh && source.supports_input(), reason: if !self.fresh { "Catalog unavailable · previous observation".into() } else if !source.supports_input() { "This node advertises no single-planar format compatible with the qualified input.".into() } else { String::new() }, vendor_id: format!("{:04x}", identity.vendor_id()), product_id: format!("{:04x}", identity.product_id()), topology: topology(identity), serial: identity.serial().map(str::to_owned), node_label: source.node.devnode().display().to_string() });
        }
        for (index, source) in self.audio.iter().enumerate() {
            projection.audio_input_rows.push(AudioInputRow {
                key: format!("input:{}:{index}", self.revision),
                label: if source.description.is_empty() {
                    source.identity.name().into()
                } else {
                    source.description.clone()
                },
                eligible: self.audio_fresh,
                reason: if self.audio_fresh {
                    String::new()
                } else {
                    "Audio catalog unavailable".into()
                },
            });
        }
        self.formats.clear();
        self.sizes.clear();
        self.rates.clear();
        if let Some(source) = source {
            let auto_available =
                source
                    .node
                    .capabilities()
                    .fourcc_capabilities()
                    .iter()
                    .any(|format| {
                        source.supports_format(format.captured_fourcc)
                            && eligible_format(InputFormatPolicy::Auto, format.captured_fourcc)
                    });
            projection.format_rows.push(FormatRow {
                key: "auto".into(),
                label: "Auto · YUYV / NV12 / MJPG".into(),
                fourcc: String::new(),
                eligible: self.fresh && auto_available,
                reason: if !self.fresh {
                    "Catalog unavailable".into()
                } else if !auto_available {
                    "No format from the Auto policy is advertised on this node.".into()
                } else {
                    String::new()
                },
            });
            for format in source.node.capabilities().fourcc_capabilities() {
                let index = self.formats.len();
                self.formats.push(format.captured_fourcc);
                let label = fourcc_text(format.captured_fourcc);
                let mapped = crate::capture::input::supports_fourcc(format.captured_fourcc);
                let single_planar = source.supports_format(format.captured_fourcc);
                projection.format_rows.push(FormatRow { key: format!("format:{}:{index}", self.revision), label: label.clone(), fourcc: label, eligible: self.fresh && mapped && single_planar, reason: if !self.fresh { "Catalog unavailable".into() } else if !mapped { "Format advertised, but with no match in the qualified lavf input.".into() } else if !single_planar { "This format is advertised only on a multi-planar queue, not admitted by the qualified input.".into() } else { String::new() } });
            }
            projection.capability_status = if self.fresh { "ready" } else { "unavailable" }.into();
            projection.capability_reason.clear();
            let needs_physical_confirmation = self.snapshot.as_ref().is_some_and(|snapshot| {
                matches!(
                    crate::domain::capture::resolve_identity(
                        source.device.identity(),
                        snapshot.devices().iter().map(|device| device.identity())
                    ),
                    Err(
                        crate::domain::capture::IdentityError::DuplicateSerial { .. }
                            | crate::domain::capture::IdentityError::SeriallessDuplicates { .. }
                    )
                )
            });
            let mut queries = Vec::new();
            let mut size_supported = false;
            let mut rate_supported = false;
            let mut unknown = false;
            let mut has_policy_format = false;
            for (format_index, capabilities) in source
                .node
                .capabilities()
                .fourcc_capabilities()
                .iter()
                .enumerate()
            {
                if !eligible_format(policy, capabilities.captured_fourcc)
                    || !source.supports_format(capabilities.captured_fourcc)
                    || !crate::capture::input::supports_fourcc(capabilities.captured_fourcc)
                {
                    continue;
                }
                has_policy_format = true;
                let format_key = format!("format:{}:{format_index}", self.revision);
                let Some(sizes) = capabilities.sizes.as_option() else {
                    unknown = true;
                    continue;
                };
                let exact_intervals = match sizes.kind() {
                    FrameSizeKind::Discrete(entries) => {
                        for entry in entries {
                            if !self.sizes.contains(&entry.size) {
                                self.sizes.push(entry.size);
                            }
                        }
                        size.and_then(|size| entries.iter().find(|entry| entry.size == size))
                            .map(|entry| &entry.intervals)
                    }
                    FrameSizeKind::Stepwise {
                        min,
                        max,
                        step_width,
                        step_height,
                    } => {
                        projection.size_ranges.push(SizeRange {
                            format_key: format_key.clone(),
                            kind: "stepwise",
                            min: (*min).into(),
                            max: (*max).into(),
                            step_width: Some(step_width.get()),
                            step_height: Some(step_height.get()),
                        });
                        None
                    }
                    FrameSizeKind::Continuous { min, max } => {
                        projection.size_ranges.push(SizeRange {
                            format_key: format_key.clone(),
                            kind: "continuous",
                            min: (*min).into(),
                            max: (*max).into(),
                            step_width: None,
                            step_height: None,
                        });
                        None
                    }
                };
                if let Some(size) = size
                    && sizes.contains(size)
                {
                    size_supported = true;
                    let intervals = if exact_intervals.is_some() {
                        exact_intervals
                    } else {
                        if self.interval_refresh {
                            queries.push(capabilities.captured_fourcc);
                        }
                        match self.intervals.as_ref().and_then(|result| {
                            result
                                .values
                                .iter()
                                .find(|(fourcc, _)| *fourcc == capabilities.captured_fourcc)
                        }) {
                            Some((_, Ok(intervals))) => Some(intervals),
                            Some((_, Err(error))) => {
                                unknown = true;
                                projection.capability_reason = error.clone();
                                None
                            }
                            None => {
                                if !queries.contains(&capabilities.captured_fourcc) {
                                    queries.push(capabilities.captured_fourcc);
                                }
                                None
                            }
                        }
                    };
                    match intervals {
                        Some(Descriptor::Available(intervals)) => {
                            if rate.is_some_and(|rate| intervals.supports(rate)) {
                                rate_supported = true;
                            }
                            match intervals.kind() {
                                FrameIntervalKind::Discrete(values) => {
                                    for interval in values {
                                        let rate = interval.rate();
                                        if !self.rates.contains(&rate) {
                                            self.rates.push(rate);
                                        }
                                    }
                                }
                                FrameIntervalKind::Stepwise { min, max, step } => {
                                    projection.interval_ranges.push(IntervalRange {
                                        format_key,
                                        kind: "stepwise",
                                        min: rational(*min),
                                        max: rational(*max),
                                        step: Some(rational(*step)),
                                    })
                                }
                                FrameIntervalKind::Continuous { min, max } => {
                                    projection.interval_ranges.push(IntervalRange {
                                        format_key,
                                        kind: "continuous",
                                        min: rational(*min),
                                        max: rational(*max),
                                        step: None,
                                    })
                                }
                            }
                        }
                        Some(Descriptor::NotReported) => unknown = true,
                        None => {}
                    }
                }
            }
            if !queries.is_empty() && self.fresh {
                if !self.query_pending
                    && self.mode_pending.is_none()
                    && let (Some(worker), Some(context)) = (&self.worker, context)
                {
                    match self.query_sequence.checked_add(1) {
                        Some(serial) => {
                            self.query_sequence = serial;
                            worker.query(Query {
                                serial,
                                context,
                                source,
                                formats: queries,
                            });
                            self.query_pending = true;
                        }
                        None => {
                            self.capture_error = "exact interval request sequence exhausted".into();
                            self.fresh = false;
                        }
                    }
                }
                projection.capability_status = "querying".into();
                projection.capability_reason = "Reading exact intervals.".into();
            } else if unknown && self.fresh {
                projection.capability_status = "unknown".into();
                if projection.capability_reason.is_empty() {
                    projection.capability_reason =
                        "Descriptors not reported · exact validation is still required.".into();
                }
            } else if !has_policy_format {
                projection.capability_reason = match policy {
                InputFormatPolicy::Auto => "No format from the Auto policy is advertised on a single-planar queue; no RGB or unknown fallback.",
                InputFormatPolicy::Manual { fourcc } if !crate::capture::input::supports_fourcc(fourcc) => "The retained manual format has no match in the qualified lavf input.",
                InputFormatPolicy::Manual { .. } => "The retained manual format is not advertised on a single-planar queue of this node.",
            }.into();
            } else if size.is_some() && !size_supported {
                projection.capability_reason =
                    "The retained resolution is not advertised for this format on this node."
                        .into();
            } else if rate.is_some() && !rate_supported {
                projection.capability_reason =
                    "The retained Framerate is not advertised for this exact resolution.".into();
            }
            if !self.fresh {
                projection.capability_reason =
                    "Previous observation only · catalog unavailable.".into();
            } else if needs_physical_confirmation {
                if !projection.capability_reason.is_empty() {
                    projection.capability_reason.push('\n');
                }
                projection.capability_reason.push_str("Ambiguous USB identity · a physical recovery confirmation is required; this row choice does not yet admit capture.");
            }
            self.project_modes(&mut projection, size);
        }
        if !self.fresh && projection.capability_status != "unavailable" {
            projection.capability_status = "unavailable".into();
            projection.capability_reason = self.capture_error.clone();
            projection.error = [
                self.worker_error.as_str(),
                self.capture_error.as_str(),
                self.audio_error.as_str(),
            ]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
            for row in &mut projection.source_rows {
                row.eligible = false;
                row.reason = "Catalog unavailable · previous observation".into();
            }
            for row in &mut projection.format_rows {
                row.eligible = false;
                row.reason = "Catalog unavailable".into();
            }
        }
        projection.busy |= self.query_pending || self.mode_pending.is_some();
        self.projection = projection;
    }
    #[cfg(test)]
    pub(super) fn fixture(
        snapshot: linux::CaptureSnapshot,
        audio: Vec<audio::AudioSource>,
    ) -> Self {
        let snapshot = Arc::new(snapshot);
        let sources = Source::snapshot_rows(&snapshot);
        Self {
            sources,
            snapshot: Some(snapshot),
            audio,
            fresh: true,
            audio_fresh: true,
            ..Self::default()
        }
    }
    #[cfg(test)]
    pub(super) fn fixture_with_audio(
        snapshot: linux::CaptureSnapshot,
        observations: Vec<audio::CatalogSource>,
    ) -> Self {
        let (audio, audio_usb) = observations
            .into_iter()
            .map(|row| (row.source, row.usb))
            .unzip();
        let mut model = Self::fixture(snapshot, audio);
        model.audio_usb = audio_usb;
        model
    }
    #[cfg(test)]
    pub(super) fn complete_mode_fixture(&mut self, reply: ModeReply) {
        self.mode_reply = Some(reply);
    }
    /// Keep real mode-admission/slot semantics without starting discovery or I/O.
    #[cfg(test)]
    pub(super) fn hold_mode_requests_fixture(&mut self) {
        assert!(self.worker.is_none());
        self.worker = Some(Worker {
            shared: Arc::new(Shared {
                slots: Mutex::new(Slots::default()),
                ready: Condvar::new(),
            }),
            handle: None,
        });
    }
}
fn rational(value: FrameInterval) -> Rational {
    Rational {
        numerator: value.numerator(),
        denominator: value.denominator(),
    }
}
pub(super) fn topology(identity: &DeviceIdentity) -> String {
    format!(
        "{}:{}",
        identity.topology().controller(),
        identity
            .topology()
            .ports()
            .iter()
            .map(|port| port.get().to_string())
            .collect::<Vec<_>>()
            .join(".")
    )
}
pub(super) fn fourcc_text(fourcc: CapturedFourCc) -> String {
    String::from_utf8_lossy(&fourcc.bytes()).into_owned()
}
fn eligible_format(policy: InputFormatPolicy, fourcc: CapturedFourCc) -> bool {
    match policy {
        InputFormatPolicy::Manual { fourcc: selected } => fourcc == selected,
        InputFormatPolicy::Auto => [*b"YUYV", *b"NV12", *b"MJPG"].contains(&fourcc.bytes()),
    }
}
pub(super) fn decimal(value: &str) -> Option<u64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        None
    } else {
        value.parse().ok()
    }
}
fn row_index(key: &str, prefix: &str, revision: u64) -> Result<usize, String> {
    let mut parts = key.split(':');
    if parts.next() != Some(prefix) || parts.next().and_then(decimal) != Some(revision) {
        return Err("row revision is stale".into());
    }
    let index = parts
        .next()
        .and_then(decimal)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or("malformed row key")?;
    if parts.next().is_some() {
        return Err("malformed row key".into());
    }
    Ok(index)
}

fn size_cmp(a: FrameSize, b: FrameSize) -> std::cmp::Ordering {
    (
        u64::from(a.width()) * u64::from(a.height()),
        a.width(),
        a.height(),
    )
        .cmp(&(
            u64::from(b.width()) * u64::from(b.height()),
            b.width(),
            b.height(),
        ))
}
fn rate_cmp(a: FrameRate, b: FrameRate) -> std::cmp::Ordering {
    (u64::from(a.numerator()) * u64::from(b.denominator()))
        .cmp(&(u64::from(b.numerator()) * u64::from(a.denominator())))
}
fn largest_size(kind: &FrameSizeKind) -> FrameSize {
    match kind {
        FrameSizeKind::Discrete(entries) => entries
            .iter()
            .map(|entry| entry.size)
            .max_by(|a, b| size_cmp(*a, *b))
            .expect("validated nonempty sizes"),
        FrameSizeKind::Continuous { max, .. } => *max,
        FrameSizeKind::Stepwise {
            min,
            max,
            step_width,
            step_height,
        } => {
            let lattice = |min, max, step| min + ((max - min) / step) * step;
            FrameSize::new(
                lattice(min.width(), max.width(), step_width.get()),
                lattice(min.height(), max.height(), step_height.get()),
            )
            .expect("valid nonzero lattice point within descriptor bounds")
        }
    }
}
fn maximum_rate(intervals: &FrameIntervals) -> FrameRate {
    match intervals.kind() {
        FrameIntervalKind::Discrete(values) => values
            .iter()
            .map(|interval| interval.rate())
            .max_by(|a, b| rate_cmp(*a, *b))
            .expect("validated nonempty intervals"),
        FrameIntervalKind::Stepwise { min, .. } | FrameIntervalKind::Continuous { min, .. } => {
            min.rate()
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManualEligibility {
    Eligible,
    Absent,
    Unknown,
}
/// Absence of an announced, mapped single-planar format is proof. Missing
/// descriptors are not proof of absence and must never enable Auto fallback.
fn manual_eligibility(
    capabilities: &crate::domain::capture::NodeCapabilities,
    fourcc: CapturedFourCc,
) -> ManualEligibility {
    if !crate::capture::input::supports_fourcc(fourcc)
        || !capabilities.formats().iter().any(|format| {
            format.captured_fourcc == fourcc
                && format.buffer_type == crate::domain::capture::CaptureBufferType::SinglePlanar
        })
    {
        return ManualEligibility::Absent;
    }
    let Some(sizes) = capabilities
        .fourcc_capabilities()
        .iter()
        .find(|format| format.captured_fourcc == fourcc)
        .and_then(|format| format.sizes.as_option())
    else {
        return ManualEligibility::Unknown;
    };
    match sizes.kind() {
        FrameSizeKind::Discrete(entries)
            if entries
                .iter()
                .all(|entry| entry.intervals.as_option().is_none()) =>
        {
            ManualEligibility::Unknown
        }
        _ => ManualEligibility::Eligible,
    }
}
fn resolve_mode_with_intent(
    capabilities: &crate::domain::capture::NodeCapabilities,
    mut policy: InputFormatPolicy,
    prior: Option<CaptureModeRequest>,
    intent: ModeIntent,
    query: impl FnMut(CapturedFourCc, FrameSize) -> Result<Option<Descriptor<FrameIntervals>>, String>,
) -> Result<Option<CaptureModeRequest>, String> {
    if intent == ModeIntent::RestoreProfile
        && let InputFormatPolicy::Manual { fourcc } = policy
    {
        match manual_eligibility(capabilities, fourcc) {
            ManualEligibility::Absent => policy = InputFormatPolicy::Auto,
            ManualEligibility::Unknown => {
                return Err("The manual format's capabilities are not reported.".into());
            }
            ManualEligibility::Eligible => {}
        }
    }
    resolve_mode(capabilities, policy, prior, query)
}

/// None means exact ranged ioctls are required on the existing worker. Unknown
/// descriptors and ioctl errors are never interpreted as incompatibility.
fn resolve_mode(
    capabilities: &crate::domain::capture::NodeCapabilities,
    policy: InputFormatPolicy,
    prior: Option<CaptureModeRequest>,
    mut query: impl FnMut(
        CapturedFourCc,
        FrameSize,
    ) -> Result<Option<Descriptor<FrameIntervals>>, String>,
) -> Result<Option<CaptureModeRequest>, String> {
    let eligible = |fourcc| {
        eligible_format(policy, fourcc)
            && capabilities.formats().iter().any(|format| {
                format.captured_fourcc == fourcc
                    && format.buffer_type == crate::domain::capture::CaptureBufferType::SinglePlanar
            })
            && crate::capture::input::supports_fourcc(fourcc)
    };
    let formats = capabilities.fourcc_capabilities();
    let mut largest = None;
    let mut unknown_sizes = false;
    for format in formats
        .iter()
        .filter(|format| eligible(format.captured_fourcc))
    {
        let Some(sizes) = format.sizes.as_option() else {
            unknown_sizes = true;
            continue;
        };
        let size = largest_size(sizes.kind());
        if largest.is_none_or(|current| size_cmp(size, current).is_gt()) {
            largest = Some(size);
        }
    }
    if let Some(prior) = prior {
        let mut unresolved = false;
        let mut unknown = unknown_sizes;
        let mut maximum = None;
        for format in formats
            .iter()
            .filter(|format| eligible(format.captured_fourcc))
        {
            let Some(sizes) = format.sizes.as_option() else {
                continue;
            };
            if !sizes.contains(prior.size) {
                continue;
            }
            let queried;
            let intervals = match sizes.kind() {
                FrameSizeKind::Discrete(entries) => {
                    &entries
                        .iter()
                        .find(|entry| entry.size == prior.size)
                        .expect("contains")
                        .intervals
                }
                _ => {
                    queried = query(format.captured_fourcc, prior.size)?;
                    let Some(intervals) = queried.as_ref() else {
                        unresolved = true;
                        continue;
                    };
                    intervals
                }
            };
            match intervals {
                Descriptor::Available(intervals) => {
                    if intervals.supports(prior.rate) {
                        return Ok(Some(CaptureModeRequest {
                            format_policy: policy,
                            ..prior
                        }));
                    }
                    let rate = maximum_rate(intervals);
                    if maximum.is_none_or(|current| rate_cmp(rate, current).is_gt()) {
                        maximum = Some(rate);
                    }
                }
                Descriptor::NotReported => unknown = true,
            }
        }
        if unresolved {
            return Ok(None);
        }
        if unknown {
            return Err("The current mode's compatibility is not reported by this node.".into());
        }
        // Reuse the just-read exact intervals if the incompatible old mode was
        // already at the largest size; never issue the same ioctl twice.
        if Some(prior.size) == largest
            && let Some(rate) = maximum
        {
            return Ok(Some(CaptureModeRequest {
                format_policy: policy,
                size: prior.size,
                rate,
            }));
        }
    }
    if unknown_sizes {
        return Err("The resolutions of this format are not reported.".into());
    }
    let size =
        largest.ok_or("No single-planar mode compatible with the retained format policy.")?;
    let mut maximum = None;
    let mut unresolved = false;
    for format in formats
        .iter()
        .filter(|format| eligible(format.captured_fourcc))
    {
        let sizes = format.sizes.as_option().expect("checked above");
        if !sizes.contains(size) {
            continue;
        }
        let queried;
        let intervals = match sizes.kind() {
            FrameSizeKind::Discrete(entries) => {
                &entries
                    .iter()
                    .find(|entry| entry.size == size)
                    .expect("contains")
                    .intervals
            }
            _ => {
                queried = query(format.captured_fourcc, size)?;
                let Some(intervals) = queried.as_ref() else {
                    unresolved = true;
                    continue;
                };
                intervals
            }
        };
        let intervals = intervals
            .as_option()
            .ok_or("The maximum Framerate of the largest resolution is not reported.")?;
        let rate = maximum_rate(intervals);
        if maximum.is_none_or(|current| rate_cmp(rate, current).is_gt()) {
            maximum = Some(rate);
        }
    }
    if unresolved {
        return Ok(None);
    }
    Ok(Some(CaptureModeRequest {
        format_policy: policy,
        size,
        rate: maximum.ok_or("Exact intervals unavailable for the largest resolution.")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn capabilities(kind: FrameSizeKind) -> crate::domain::capture::NodeCapabilities {
        use crate::domain::capture::{
            CaptureBufferType, FormatDescriptor, FourCcCapabilities, FrameSizes, NodeCapabilities,
        };
        NodeCapabilities::new(
            vec![FormatDescriptor {
                buffer_type: CaptureBufferType::SinglePlanar,
                captured_fourcc: mode().captured_fourcc,
                description: "fixture".into(),
                flags: 0,
            }],
            vec![FourCcCapabilities {
                captured_fourcc: mode().captured_fourcc,
                sizes: Descriptor::Available(FrameSizes::new(kind).unwrap()),
            }],
        )
        .unwrap()
    }
    fn intervals(rates: &[(u32, u32)]) -> Descriptor<FrameIntervals> {
        Descriptor::Available(
            FrameIntervals::new(FrameIntervalKind::Discrete(
                rates
                    .iter()
                    .map(|(n, d)| FrameRate::new(*n, *d).unwrap().interval())
                    .collect(),
            ))
            .unwrap(),
        )
    }
    #[test]
    fn exact_ordering_normalizes_rationals_and_handles_u32_cross_products() {
        let sixty = FrameRate::new(60, 1).unwrap();
        let ntsc = FrameRate::new(60000, 1001).unwrap();
        assert!(rate_cmp(sixty, ntsc).is_gt());
        assert_eq!(
            rate_cmp(FrameRate::new(120000, 2002).unwrap(), ntsc),
            std::cmp::Ordering::Equal
        );
        let high = FrameRate::new(u32::MAX, u32::MAX - 1).unwrap();
        let low = FrameRate::new(u32::MAX - 1, u32::MAX).unwrap();
        assert!(rate_cmp(high, low).is_gt());
        assert!(
            size_cmp(
                FrameSize::new(u32::MAX, u32::MAX).unwrap(),
                FrameSize::new(u32::MAX, u32::MAX - 1).unwrap()
            )
            .is_gt()
        );
        assert!(
            size_cmp(
                FrameSize::new(1920, 1080).unwrap(),
                FrameSize::new(1080, 1920).unwrap()
            )
            .is_gt()
        );
    }
    #[test]
    fn sorted_backing_values_resolve_their_own_revision_keys() {
        let mut model = Model {
            fresh: true,
            ..Model::default()
        };
        model.sizes = vec![
            FrameSize::new(640, 480).unwrap(),
            FrameSize::new(1920, 1080).unwrap(),
            FrameSize::new(1280, 720).unwrap(),
        ];
        model.rates = vec![
            FrameRate::new(60000, 1001).unwrap(),
            FrameRate::new(30, 1).unwrap(),
            FrameRate::new(60, 1).unwrap(),
        ];
        let mut projection = Catalog::default();
        model.project_modes(&mut projection, None);
        model.projection = projection;
        assert_eq!(
            model
                .projection
                .size_rows
                .iter()
                .map(|row| row.width)
                .collect::<Vec<_>>(),
            vec![1920, 1280, 640]
        );
        assert_eq!(
            model
                .projection
                .rate_rows
                .iter()
                .map(|row| row.exact.as_str())
                .collect::<Vec<_>>(),
            vec!["60/1", "60000/1001", "30/1"]
        );
        for row in &model.projection.size_rows {
            assert_eq!(
                model.resolve_size("1", &row.key).unwrap(),
                FrameSize::new(row.width, row.height).unwrap()
            );
        }
        for row in &model.projection.rate_rows {
            assert_eq!(
                model.resolve_rate("1", &row.key).unwrap(),
                FrameRate::new(row.numerator, row.denominator).unwrap()
            );
        }
    }
    #[test]
    fn discrete_source_preserves_compatible_manual_mode_else_largest_exact_maximum() {
        use crate::domain::capture::DiscreteSize;
        let small = FrameSize::new(640, 480).unwrap();
        let large = FrameSize::new(1920, 1080).unwrap();
        let caps = capabilities(FrameSizeKind::Discrete(vec![
            DiscreteSize {
                size: small,
                intervals: intervals(&[(120, 1), (60, 1)]),
            },
            DiscreteSize {
                size: large,
                intervals: intervals(&[(60000, 1001), (60, 1)]),
            },
        ]));
        let manual = InputFormatPolicy::Manual {
            fourcc: mode().captured_fourcc,
        };
        let prior = CaptureModeRequest {
            format_policy: manual,
            size: small,
            rate: FrameRate::new(60, 1).unwrap(),
        };
        assert_eq!(
            resolve_mode(&caps, manual, Some(prior), |_, _| panic!(
                "discrete needs no ioctl"
            ))
            .unwrap(),
            Some(prior)
        );
        let incompatible = CaptureModeRequest {
            rate: FrameRate::new(30, 1).unwrap(),
            ..prior
        };
        let selected = resolve_mode(&caps, manual, Some(incompatible), |_, _| {
            panic!("discrete needs no ioctl")
        })
        .unwrap()
        .unwrap();
        assert_eq!(selected.format_policy, manual);
        assert_eq!(selected.size, large);
        assert_eq!(selected.rate, FrameRate::new(60, 1).unwrap());
        let auto = resolve_mode(&caps, InputFormatPolicy::Auto, None, |_, _| {
            panic!("discrete needs no ioctl")
        })
        .unwrap()
        .unwrap();
        assert_eq!(auto.format_policy, InputFormatPolicy::Auto);
        assert_eq!(auto.size, large);
        assert_eq!(auto.rate, selected.rate);
        let unavailable = InputFormatPolicy::Manual {
            fourcc: CapturedFourCc::from_bytes(*b"YUYV"),
        };
        assert!(
            resolve_mode(&caps, unavailable, Some(prior), |_, _| panic!(
                "must not substitute a format"
            ))
            .is_err()
        );
    }
    #[test]
    fn ranged_source_queries_current_before_valid_lattice_maximum_once() {
        let min = FrameSize::new(640, 360).unwrap();
        let max = FrameSize::new(3841, 2161).unwrap();
        let caps = capabilities(FrameSizeKind::Stepwise {
            min,
            max,
            step_width: std::num::NonZeroU32::new(16).unwrap(),
            step_height: std::num::NonZeroU32::new(9).unwrap(),
        });
        let prior = CaptureModeRequest {
            format_policy: InputFormatPolicy::Auto,
            size: FrameSize::new(1280, 720).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        let mut queried = Vec::new();
        let result = resolve_mode(&caps, InputFormatPolicy::Auto, Some(prior), |_, size| {
            queried.push(size);
            Ok(Some(if size == prior.size {
                intervals(&[(30, 1)])
            } else {
                Descriptor::Available(
                    FrameIntervals::new(FrameIntervalKind::Stepwise {
                        min: FrameInterval::new(1, 120).unwrap(),
                        max: FrameInterval::new(1, 15).unwrap(),
                        step: FrameInterval::new(1, 120).unwrap(),
                    })
                    .unwrap(),
                )
            }))
        })
        .unwrap()
        .unwrap();
        assert_eq!(result.size, FrameSize::new(3840, 2160).unwrap());
        assert_eq!(result.rate, FrameRate::new(120, 1).unwrap());
        assert_eq!(queried, vec![prior.size, result.size]);
        let mut calls = 0;
        let same_size = CaptureModeRequest {
            size: result.size,
            rate: FrameRate::new(200, 1).unwrap(),
            ..prior
        };
        let selected = resolve_mode(&caps, InputFormatPolicy::Auto, Some(same_size), |_, _| {
            calls += 1;
            Ok(Some(intervals(&[(120, 1)])))
        })
        .unwrap()
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(selected.rate, FrameRate::new(120, 1).unwrap());
        let preserved = resolve_mode(&caps, InputFormatPolicy::Auto, Some(prior), |_, size| {
            assert_eq!(size, prior.size);
            Ok(Some(intervals(&[(60, 1)])))
        })
        .unwrap()
        .unwrap();
        assert_eq!(preserved, prior);
    }
    #[test]
    fn unknown_largest_intervals_and_query_failures_never_downgrade() {
        use crate::domain::capture::DiscreteSize;
        let caps = capabilities(FrameSizeKind::Discrete(vec![
            DiscreteSize {
                size: FrameSize::new(640, 480).unwrap(),
                intervals: intervals(&[(60, 1)]),
            },
            DiscreteSize {
                size: mode().size,
                intervals: Descriptor::NotReported,
            },
        ]));
        assert!(
            resolve_mode(&caps, InputFormatPolicy::Auto, None, |_, _| panic!(
                "no query for discrete"
            ))
            .is_err()
        );
        let continuous = capabilities(FrameSizeKind::Continuous {
            min: FrameSize::new(640, 360).unwrap(),
            max: FrameSize::new(3840, 2160).unwrap(),
        });
        assert!(
            resolve_mode(&continuous, InputFormatPolicy::Auto, None, |_, _| Ok(None))
                .unwrap()
                .is_none()
        );
        let error = resolve_mode(&continuous, InputFormatPolicy::Auto, None, |_, _| {
            Err("exact ioctl failed".into())
        })
        .unwrap_err();
        assert_eq!(error, "exact ioctl failed");
        assert!(
            resolve_mode(&continuous, InputFormatPolicy::Auto, None, |_, _| Ok(Some(
                Descriptor::NotReported
            )))
            .is_err()
        );
        let mode = resolve_mode(&continuous, InputFormatPolicy::Auto, None, |_, _| {
            Ok(Some(Descriptor::Available(
                FrameIntervals::new(FrameIntervalKind::Continuous {
                    min: FrameInterval::new(1001, 60000).unwrap(),
                    max: FrameInterval::new(1, 15).unwrap(),
                })
                .unwrap(),
            )))
        })
        .unwrap()
        .unwrap();
        assert_eq!(mode.size, FrameSize::new(3840, 2160).unwrap());
        assert_eq!(mode.rate, FrameRate::new(60000, 1001).unwrap());
    }
    #[test]
    fn source_resolution_uses_one_existing_request_slot_without_replacement() {
        let mut model = Model::fixture(
            linux::session_range_fixture(&["/dev/video0"], mode()),
            Vec::new(),
        );
        let shared = Arc::new(Shared {
            slots: Mutex::new(Slots::default()),
            ready: Condvar::new(),
        });
        model.worker = Some(Worker {
            shared: shared.clone(),
            handle: None,
        });
        let physical = model.sources[0].physical();
        let ModeResolution::Pending(serial) = model
            .resolve_source_mode(&physical, InputFormatPolicy::Auto, None, ModeIntent::Strict)
            .unwrap()
        else {
            panic!("ranges require exact query");
        };
        assert!(
            model
                .resolve_source_mode(&physical, InputFormatPolicy::Auto, None, ModeIntent::Strict)
                .is_err()
        );
        let slots = shared.slots.lock();
        assert!(
            matches!(&slots.query, Some(Request::Mode(query)) if query.serial == serial && query.source.physical() == physical)
        );
        drop(slots);
        model.cancel_mode();
    }
    #[test]
    fn automatic_audio_requires_one_exact_raw_usb_ancestor_not_decorated_serial() {
        let snapshot = linux::session_fixture(&["/dev/video0"], mode());
        let identity = snapshot.devices()[0].identity().clone();
        let source = |name: &str| audio::AudioSource {
            identity: crate::domain::capture::AudioSourceIdentity::new(
                name.into(),
                vec![("device.serial".into(), "GENKI_ShadowCast_3_fixture".into())],
            )
            .unwrap(),
            description: "Identical model".into(),
        };
        let chosen = source("capture-card");
        let impostor = source("other-card");
        let mut model = Model::fixture(snapshot, vec![chosen.clone(), impostor]);
        let physical = model.sources[0].physical();
        let association = audio::UsbAssociation {
            syspath: physical.usb.clone(),
            identity,
        };
        let mut other = association.clone();
        other.syspath = "/sys/usb/other-card".into();
        model.audio_usb = vec![Ok(Some(association.clone())), Ok(Some(other.clone()))];
        assert_eq!(
            model.associated_audio(&physical),
            (
                AudioSelection::Enabled {
                    source: chosen.identity
                },
                String::new()
            )
        );
        model.audio_usb[1] = Ok(Some(association.clone()));
        let (disabled, reason) = model.associated_audio(&physical);
        assert!(!disabled.enabled() && !reason.is_empty());
        model.audio_usb = vec![Ok(Some(other.clone())), Ok(Some(other))];
        let (disabled, reason) = model.associated_audio(&physical);
        assert!(!disabled.enabled() && !reason.is_empty());
        model.audio_usb = vec![Ok(Some(association.clone())), Ok(None)];
        assert!(
            model.associated_audio(&physical).0.enabled(),
            "a proven non-USB host microphone is not a second USB input"
        );
        let wrong_identity = audio::UsbAssociation {
            identity: DeviceIdentity::new(
                physical.identity.vendor_id(),
                physical.identity.product_id(),
                physical.identity.topology().clone(),
                Some("different-raw-usb-serial".into()),
            )
            .unwrap(),
            ..association.clone()
        };
        model.audio_usb = vec![Ok(Some(wrong_identity)), Ok(None)];
        assert!(
            !model.associated_audio(&physical).0.enabled(),
            "matching syspath alone is not raw USB identity agreement"
        );
        model.audio_usb = vec![Ok(Some(association)), Err("alsa.card missing".into())];
        assert!(
            !model.associated_audio(&physical).0.enabled(),
            "an unresolved source cannot prove uniqueness"
        );
        model.audio_usb = vec![
            Err("alsa.card missing".into()),
            Err("sysfs unavailable".into()),
        ];
        let (disabled, reason) = model.associated_audio(&physical);
        assert!(!disabled.enabled() && reason.contains("alsa.card missing"));
        model.audio_fresh = false;
        model.audio_error = "Pulse discovery failed".into();
        assert!(
            model
                .associated_audio(&physical)
                .1
                .contains("Pulse discovery failed")
        );
    }
    fn mode() -> crate::domain::capture::CaptureMode {
        crate::domain::capture::CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(1920, 1080).unwrap(),
            rate: FrameRate::new(30000, 1001).unwrap(),
        }
    }
    #[test]
    fn exact_rates_and_physical_nodes_stay_distinct() {
        let mut model = Model::fixture(
            linux::session_fixture(&["/dev/video0", "/dev/video1"], mode()),
            Vec::new(),
        );
        let physical = model.sources[1].physical();
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        let key = model.source_key(&physical);
        assert_eq!(
            model
                .resolve_source(&model.revision.to_string(), &key)
                .unwrap()
                .node
                .devnode(),
            std::path::Path::new("/dev/video1")
        );
        let row = &model.projection.rate_rows[0];
        assert_eq!(
            model
                .resolve_rate(&model.revision.to_string(), &row.key)
                .unwrap(),
            FrameRate::new(30000, 1001).unwrap()
        );
        let old_size = model.projection.size_rows[0].key.clone();
        let old_revision = model.revision.to_string();
        model.rebuild(
            Some(&model.sources[0].physical()),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        assert!(model.resolve_size(&old_revision, &old_size).is_err());
    }
    #[test]
    fn unreported_exact_intervals_are_unknown_not_unsupported() {
        let mut model = Model::fixture(
            linux::session_range_fixture(&["/dev/video0"], mode()),
            Vec::new(),
        );
        let physical = model.sources[0].physical();
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        model.intervals = Some(Intervals {
            serial: 0,
            context: model.context.clone().unwrap(),
            values: vec![(mode().captured_fourcc, Ok(Descriptor::NotReported))],
        });
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        assert_eq!(model.projection.capability_status, "unknown");
        assert!(model.projection.rate_rows.is_empty());
        assert!(!model.projection.size_ranges.is_empty());
    }
    #[test]
    fn unavailable_observation_rows_are_display_only() {
        let mut model =
            Model::fixture(linux::session_fixture(&["/dev/video0"], mode()), Vec::new());
        let physical = model.sources[0].physical();
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        let revision = model.revision.to_string();
        let key = model.source_key(&physical);
        model.fresh = false;
        assert!(model.resolve_source(&revision, &key).is_err());
    }
    #[test]
    fn decimal_tokens_preserve_all_u64_bits_and_reject_aliases() {
        for value in [9_007_199_254_740_993, u64::MAX] {
            assert_eq!(decimal(&value.to_string()), Some(value));
        }
        for value in ["", "01", "+1", " 1", "1.0", "18446744073709551616", "١"] {
            assert_eq!(decimal(value), None);
        }
    }
    #[test]
    fn revision_scoped_keys_never_resolve_stale_or_malformed_rows() {
        assert_eq!(
            row_index(
                "capture:9007199254740993:1",
                "capture",
                9_007_199_254_740_993
            )
            .unwrap(),
            1
        );
        for key in ["capture:1:0", "capture:2:00", "capture:2:0:x", "format:2:0"] {
            assert!(row_index(key, "capture", 2).is_err());
        }
    }
    #[test]
    fn auto_does_not_invent_rgb_or_unknown_format_support() {
        assert!(!eligible_format(
            InputFormatPolicy::Auto,
            CapturedFourCc::from_bytes(*b"BGR3")
        ));
        assert!(!eligible_format(
            InputFormatPolicy::Auto,
            CapturedFourCc::from_bytes(*b"XXXX")
        ));
    }
    #[test]
    fn physical_paths_and_display_labels_cannot_rebind_a_replacement_identity() {
        let original = Model::fixture(linux::session_fixture(&["/dev/video0"], mode()), Vec::new());
        let source = &original.sources[0];
        let physical = source.physical();
        let identity = DeviceIdentity::new(
            source.device.identity().vendor_id(),
            source.device.identity().product_id(),
            source.device.identity().topology().clone(),
            Some("replacement-serial".into()),
        )
        .unwrap();
        let replacement = Model::fixture(
            linux::physical_fixture(&[(
                identity,
                source.node.devnode().to_str().unwrap(),
                source.node.usb_syspath().to_str().unwrap(),
                source.node.syspath().to_str().unwrap(),
                mode(),
            )]),
            Vec::new(),
        );
        assert_eq!(
            source.node.card_name(),
            replacement.sources[0].node.card_name()
        );
        assert_eq!(source.node.syspath(), replacement.sources[0].node.syspath());
        assert!(replacement.source(&physical).is_none());
        assert!(replacement.source_key(&physical).is_empty());
    }
    #[test]
    fn announced_manual_mapping_availability_is_not_auto_fallback_permission() {
        for (bytes, manual_eligible) in [(*b"BGR3", true), (*b"XXXX", false)] {
            let mut requested = mode();
            requested.captured_fourcc = CapturedFourCc::from_bytes(bytes);
            let mut model = Model::fixture(
                linux::session_fixture(&["/dev/video0"], requested),
                Vec::new(),
            );
            let physical = model.sources[0].physical();
            model.rebuild(
                Some(&physical),
                InputFormatPolicy::Auto,
                Some(requested.size),
                Some(requested.rate),
                &AudioSelection::default(),
            );
            assert!(!model.projection.format_rows[0].eligible);
            assert_eq!(model.projection.format_rows[1].eligible, manual_eligible);
            assert!(model.projection.size_rows.is_empty() && model.projection.rate_rows.is_empty());
            if !manual_eligible {
                assert!(!model.projection.format_rows[1].reason.is_empty());
            }
        }
    }
    fn interval_reply(model: &Model, serial: u64, rate: FrameRate) -> Intervals {
        let context = model.context.clone().unwrap();
        let descriptor =
            linux::fixture_exact_intervals(mode().captured_fourcc, context.size, rate).intervals;
        Intervals {
            serial,
            context,
            values: vec![(mode().captured_fourcc, Ok(descriptor))],
        }
    }
    #[test]
    fn newer_exact_request_rejects_an_old_reply_even_with_the_same_size_context() {
        let mut model = Model::fixture(
            linux::session_range_fixture(&["/dev/video0"], mode()),
            Vec::new(),
        );
        let physical = model.sources[0].physical();
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        model.query_sequence = 2;
        model.query_pending = true;
        let stale = interval_reply(&model, 1, FrameRate::new(60, 1).unwrap());
        assert!(!model.accept_intervals(stale));
        assert!(model.intervals.is_none());
        let current = interval_reply(&model, 2, mode().rate);
        assert!(model.accept_intervals(current));
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        let row = &model.projection.rate_rows[0];
        assert_eq!(
            model
                .resolve_rate(&model.revision.to_string(), &row.key)
                .unwrap(),
            mode().rate
        );
    }
    #[test]
    fn range_refresh_rechecks_interior_intervals_and_rekeys_only_when_they_change() {
        let mut model = Model::fixture(
            linux::session_range_fixture(&["/dev/video0"], mode()),
            Vec::new(),
        );
        let physical = model.sources[0].physical();
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        model.query_sequence = 1;
        model.query_pending = true;
        let first = interval_reply(&model, 1, mode().rate);
        model.accept_intervals(first);
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        let revision = model.revision.to_string();
        let key = model.projection.rate_rows[0].key.clone();
        model.refresh_exact_context();
        assert!(model.interval_refresh);
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        assert!(
            model.resolve_rate(&revision, &key).is_err(),
            "previous exact observations are display-only while rechecking"
        );
        model.query_sequence = 2;
        model.query_pending = true;
        let unchanged = interval_reply(&model, 2, mode().rate);
        model.accept_intervals(unchanged);
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        assert_eq!(model.resolve_rate(&revision, &key).unwrap(), mode().rate);
        model.refresh_exact_context();
        model.query_sequence = 3;
        model.query_pending = true;
        let changed = interval_reply(&model, 3, FrameRate::new(60, 1).unwrap());
        model.accept_intervals(changed);
        model.rebuild(
            Some(&physical),
            InputFormatPolicy::Auto,
            Some(mode().size),
            Some(mode().rate),
            &AudioSelection::default(),
        );
        assert!(model.resolve_rate(&revision, &key).is_err());
        let row = &model.projection.rate_rows[0];
        assert_eq!(
            model
                .resolve_rate(&model.revision.to_string(), &row.key)
                .unwrap(),
            FrameRate::new(60, 1).unwrap()
        );
    }
    #[test]
    fn profile_restore_preserves_manual_then_falls_back_only_for_proven_absence() {
        use crate::domain::capture::DiscreteSize;
        let size = FrameSize::new(1920, 1080).unwrap();
        let caps = capabilities(FrameSizeKind::Discrete(vec![DiscreteSize {
            size,
            intervals: intervals(&[(60000, 1001), (30, 1)]),
        }]));
        let manual = InputFormatPolicy::Manual {
            fourcc: mode().captured_fourcc,
        };
        let incompatible = CaptureModeRequest {
            format_policy: manual,
            size: FrameSize::new(640, 480).unwrap(),
            rate: FrameRate::new(120, 1).unwrap(),
        };
        let adapted = resolve_mode_with_intent(
            &caps,
            manual,
            Some(incompatible),
            ModeIntent::RestoreProfile,
            |_, _| panic!("discrete proof requires no ioctl"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(adapted.format_policy, manual);
        assert_eq!(adapted.size, size);
        assert_eq!(adapted.rate, FrameRate::new(60000, 1001).unwrap());
        let absent = InputFormatPolicy::Manual {
            fourcc: CapturedFourCc::from_bytes(*b"BGR3"),
        };
        assert!(
            resolve_mode_with_intent(
                &caps,
                absent,
                Some(incompatible),
                ModeIntent::Strict,
                |_, _| panic!("discrete proof requires no ioctl"),
            )
            .is_err()
        );
        let auto = resolve_mode_with_intent(
            &caps,
            absent,
            Some(incompatible),
            ModeIntent::RestoreProfile,
            |_, _| panic!("discrete proof requires no ioctl"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(auto.format_policy, InputFormatPolicy::Auto);
        assert_eq!(auto.size, adapted.size);
        assert_eq!(auto.rate, adapted.rate);
    }

    #[test]
    fn profile_restore_unknown_descriptors_and_ioctl_errors_never_enable_auto() {
        use crate::domain::capture::{DiscreteSize, FourCcCapabilities, NodeCapabilities};
        let known = capabilities(FrameSizeKind::Discrete(vec![DiscreteSize {
            size: mode().size,
            intervals: Descriptor::NotReported,
        }]));
        let manual = InputFormatPolicy::Manual {
            fourcc: mode().captured_fourcc,
        };
        for caps in [
            known.clone(),
            NodeCapabilities::new(
                known.formats().to_vec(),
                vec![FourCcCapabilities {
                    captured_fourcc: mode().captured_fourcc,
                    sizes: Descriptor::NotReported,
                }],
            )
            .unwrap(),
        ] {
            assert_eq!(
                manual_eligibility(&caps, mode().captured_fourcc),
                ManualEligibility::Unknown
            );
            assert!(
                resolve_mode_with_intent(
                    &caps,
                    manual,
                    None,
                    ModeIntent::RestoreProfile,
                    |_, _| panic!("missing descriptors are not an ioctl authorization"),
                )
                .is_err()
            );
        }
        let ranged = capabilities(FrameSizeKind::Continuous {
            min: mode().size,
            max: FrameSize::new(1920, 1080).unwrap(),
        });
        let mut queries = 0;
        let failed = resolve_mode_with_intent(
            &ranged,
            manual,
            None,
            ModeIntent::RestoreProfile,
            |fourcc, _| {
                queries += 1;
                assert_eq!(fourcc, mode().captured_fourcc);
                Err("exact ioctl refused".into())
            },
        );
        assert!(failed.is_err());
        assert_eq!(queries, 1);
    }
}
