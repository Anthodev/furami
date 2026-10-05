//! Lifetime subscriptions and bounded, epoch-correlated recovery observations.
//! This worker never touches Qt or media. Its shared slots contain owned data;
//! validation requests a fresh subscribed scan instead of trusting cached paths.

use parking_lot::{Condvar, Mutex};
use std::{
    io::{Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use super::{
    audio,
    linux::{
        self, CaptureSnapshot,
        pulse::SourceInventorySubscription,
        udev::{Monitor, TopologyEvent},
    },
};
use crate::{
    app::ports::SubmitFailure,
    domain::{
        capture::{
            AudioError, AudioSelection, CandidateId, CaptureBufferType, DeviceIdentity,
            IdentityError, ObservationEpoch, RecoveryCandidate, RecoveryObservation,
            RecoveryWatchTarget, SelectedRouteAuthorization, SelectionToken, SourcePresence,
            SupportVerdict, VideoPresence, WatchStamp, resolve_identity,
        },
        failure::{ApplyFailure, Cause, FailureCategory, Stage, ValidationLayer},
        state::{DraftSettings, ValidationRequest},
    },
};

struct Wake {
    writer: UnixStream,
    pending: AtomicBool,
}
impl Wake {
    fn signal(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            let mut writer = &self.writer;
            if writer.write(&[1]).is_err() {
                self.pending.store(false, Ordering::Release);
            }
        }
    }
    fn drain(&self, reader: &mut UnixStream) {
        let mut bytes = [0; 32];
        while reader.read(&mut bytes).is_ok_and(|count| count != 0) {}
        self.pending.store(false, Ordering::Release);
    }
}

pub(super) struct FreshScan {
    pub target: Arc<RecoveryWatchTarget>,
    pub observation: RecoveryObservation,
    pub snapshot: Result<Arc<CaptureSnapshot>, ApplyFailure>,
    pub(super) candidates: Vec<SelectedRouteAuthorization>,
    topology_revision: u64,
    selected_context: Option<SelectionToken>,
}

#[derive(Default)]
struct Slots {
    target: Option<Arc<RecoveryWatchTarget>>,
    last_watch: u64,
    target_revision: u64,
    refresh_requested: u64,
    refresh_completed: u64,
    current: Option<Arc<FreshScan>>,
    latest: Option<RecoveryObservation>,
    consumed_choice: Option<SelectionToken>,
    consumed_epoch: Option<WatchStamp>,
    consumed_candidates: Vec<CandidateId>,
    selected_route: Option<Arc<SelectedRouteAuthorization>>,
    refresh_choices: bool,
    finished: bool,
}

impl Slots {
    fn choice_used(&self, token: SelectionToken) -> bool {
        self.consumed_epoch == Some(token.stamp)
            && self.consumed_candidates.contains(&token.candidate)
    }

    fn consume_choice(&mut self, token: SelectionToken) {
        if self.consumed_epoch != Some(token.stamp) {
            self.consumed_epoch = Some(token.stamp);
            self.consumed_candidates.clear();
        }
        if !self.consumed_candidates.contains(&token.candidate) {
            self.consumed_candidates.push(token.candidate);
        }
        self.consumed_choice = Some(token);
    }
}
struct Shared {
    slots: Mutex<Slots>,
    changed: Condvar,
    stop: Arc<AtomicBool>,
    wake: Option<Wake>,
}
#[derive(Clone)]
pub(super) struct Observer(Arc<Shared>);

impl Observer {
    pub(super) fn watch(&self, target: RecoveryWatchTarget) -> Result<(), SubmitFailure> {
        let mut slots = self.0.slots.lock();
        if slots.finished || self.0.stop.load(Ordering::Acquire) {
            return Err(SubmitFailure::Disconnected);
        }
        if slots.target.as_deref() == Some(&target) {
            return Ok(());
        }
        if target.watch.get() <= slots.last_watch {
            return Err(SubmitFailure::Disconnected);
        }
        let revision = slots
            .target_revision
            .checked_add(1)
            .ok_or(SubmitFailure::Disconnected)?;
        slots.last_watch = target.watch.get();
        slots.target = Some(Arc::new(target));
        slots.target_revision = revision;
        slots.current = None;
        slots.latest = None;
        slots.consumed_choice = None;
        slots.selected_route = None;
        slots.refresh_choices = false;
        drop(slots);
        self.signal();
        Ok(())
    }

    pub(super) fn clear(&self) {
        let mut slots = self.0.slots.lock();
        slots.target = None;
        slots.current = None;
        slots.latest = None;
        slots.selected_route = None;
        slots.refresh_choices = false;
        slots.consumed_choice = None;
        // None itself differs from every worker target; no wrapping revision.
        drop(slots);
        self.signal();
    }

    pub(super) fn poll(&self) -> Option<RecoveryObservation> {
        self.0.slots.lock().latest.take()
    }

    pub(super) fn stamp(&self, admitted: WatchStamp) -> WatchStamp {
        self.0
            .slots
            .lock()
            .current
            .as_ref()
            .filter(|scan| scan.observation.stamp.watch == admitted.watch)
            .map_or(admitted, |scan| scan.observation.stamp)
    }

    pub(super) fn retire_choice(&self, token: SelectionToken) {
        let mut slots = self.0.slots.lock();
        if slots
            .selected_route
            .as_ref()
            .is_some_and(|route| route.token == token)
        {
            slots.selected_route = None;
        }
        if slots.current.as_ref().is_some_and(|scan| {
            scan.observation.stamp == token.stamp
                && scan
                    .candidates
                    .iter()
                    .any(|candidate| candidate.token == token)
        }) {
            slots.consume_choice(token);
        }
        drop(slots);
        self.signal();
    }

    fn signal(&self) {
        if let Some(wake) = &self.0.wake {
            wake.signal();
        }
        self.0.changed.notify_all();
    }

    /// One accepted validator waits here, not on Qt. The selection token is
    /// consumed before requesting a new scan, then exact physical locators are
    /// rechecked by tuple validation. Any concurrent loss makes it stale.
    pub(super) fn fresh(
        &self,
        request: &ValidationRequest,
    ) -> Result<(Arc<FreshScan>, Option<SelectedRouteAuthorization>), ApplyFailure> {
        let mut slots = self.0.slots.lock();
        let fail = |detail| observation_failure(&request.settings, detail);
        if slots
            .target
            .as_ref()
            .is_none_or(|target| target.watch != request.watch.watch)
        {
            return Err(fail(
                "validation target is stale or has not been subscribed",
            ));
        }
        if let Some(current) = &slots.current
            && request.watch != current.observation.stamp
        {
            return Err(fail("validation observation epoch is stale"));
        }
        let authorization = if let Some(token) = request.choice {
            if slots.target.as_ref().is_none_or(|target| {
                target.video != request.settings.video || target.audio != request.settings.audio
            }) {
                return Err(fail(
                    "selection token belongs to a different requested tuple",
                ));
            }
            let current = slots
                .current
                .as_ref()
                .ok_or_else(|| fail("selection has no authoritative observation"))?;
            if token.stamp != current.observation.stamp
                || token.stamp != request.watch
                || slots.choice_used(token)
            {
                return Err(fail("selection token is stale or was already consumed"));
            }
            let selected = current
                .candidates
                .iter()
                .find(|candidate| candidate.token == token)
                .cloned()
                .ok_or_else(|| fail("selection candidate no longer exists"))?;
            slots.consume_choice(token);
            slots.refresh_choices = false;
            slots.selected_route = Some(Arc::new(selected.clone()));
            Some(selected)
        } else {
            slots.refresh_choices = slots.consumed_choice.is_some()
                && slots.current.as_ref().is_some_and(|scan| {
                    matches!(scan.observation.video, VideoPresence::Ambiguous(_))
                });
            // Retirement changes only this operation's authorization, not the
            // saved identity or the strict automatic resolver.
            slots.selected_route = None;
            None
        };
        let choice_revision = request
            .choice
            .and_then(|_| slots.current.as_ref().map(|scan| scan.topology_revision));
        let refresh = slots
            .refresh_requested
            .checked_add(1)
            .ok_or_else(|| fail("observation request counter exhausted"))?;
        slots.refresh_requested = refresh;
        self.signal();
        loop {
            if self.0.stop.load(Ordering::Acquire) || slots.finished {
                return Err(fail("observation worker stopped before fresh validation"));
            }
            if let Some(selected) = &authorization
                && slots.selected_route.as_ref().map(|route| route.token) != Some(selected.token)
            {
                return Err(fail(
                    "selected authorization retired before terminal validation",
                ));
            }
            if slots
                .target
                .as_ref()
                .is_none_or(|target| target.watch != request.watch.watch)
            {
                return Err(fail("validation watch was replaced"));
            }
            if slots.refresh_completed >= refresh
                && let Some(scan) = &slots.current
            {
                if let Some(epoch) = scan.observation.last_video_removal
                    && epoch.get() > request.watch.epoch.get()
                {
                    return Err(fail(
                        "physical removal invalidated the admitted validation epoch",
                    ));
                }
                if choice_revision.is_some_and(|revision| revision != scan.topology_revision) {
                    return Err(fail(
                        "physical topology changed since the selected authorization epoch",
                    ));
                }
                return Ok((Arc::clone(scan), authorization));
            }
            self.0.changed.wait(&mut slots);
        }
    }
}

pub(super) struct ObservationWorker {
    pub(super) observer: Observer,
    handle: Option<JoinHandle<()>>,
}
impl ObservationWorker {
    pub(super) fn new() -> Self {
        Self::spawn(None)
    }
    #[cfg(test)]
    pub(super) fn fixture() -> Self {
        Self::fixture_with_audio_catalog(Vec::new())
    }
    #[cfg(test)]
    pub(super) fn fixture_with_audio_catalog(catalog: Vec<audio::AudioSource>) -> Self {
        Self::spawn(Some(catalog))
    }
    fn spawn(fixture_catalog: Option<Vec<audio::AudioSource>>) -> Self {
        let pair = UnixStream::pair().and_then(|(reader, writer)| {
            reader.set_nonblocking(true)?;
            writer.set_nonblocking(true)?;
            Ok((reader, writer))
        });
        let (reader, wake) = match pair {
            Ok((reader, writer)) => (
                Some(reader),
                Some(Wake {
                    writer,
                    pending: AtomicBool::new(false),
                }),
            ),
            Err(_) => (None, None),
        };
        let observer = Observer(Arc::new(Shared {
            slots: Mutex::new(Slots::default()),
            changed: Condvar::new(),
            stop: Arc::new(AtomicBool::new(false)),
            wake,
        }));
        let worker = observer.clone();
        let handle = reader.and_then(|reader| {
            std::thread::Builder::new()
                .name("furami-capture-observation".into())
                .spawn(move || {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(worker.clone(), reader, fixture_catalog);
                    }));
                    let mut slots = worker.0.slots.lock();
                    if !worker.0.stop.load(Ordering::Acquire)
                        && slots.current.as_ref().is_none_or(|scan| {
                            !matches!(scan.observation.video, VideoPresence::Unknown(_))
                        })
                        && let Some(target) = slots.target.clone()
                    {
                        let detail = if outcome.is_err() {
                            "observation worker panicked"
                        } else {
                            "observation subscription worker stopped unexpectedly"
                        };
                        let stamp = slots
                            .current
                            .as_ref()
                            .map(|scan| scan.observation.stamp)
                            .unwrap_or(WatchStamp {
                                watch: target.watch,
                                epoch: ObservationEpoch::new(1).unwrap(),
                            });
                        slots.latest = Some(RecoveryObservation {
                            stamp,
                            video: VideoPresence::Unknown(observation_failure(
                                &settings(&target),
                                detail,
                            )),
                            audio: SourcePresence::Unknown(AudioError::Unavailable(detail.into())),
                            last_video_removal: slots
                                .current
                                .as_ref()
                                .and_then(|scan| scan.observation.last_video_removal),
                        });
                    }
                    slots.finished = true;
                    drop(slots);
                    worker.0.changed.notify_all();
                })
                .ok()
        });
        if handle.is_none() {
            observer.0.slots.lock().finished = true;
        }
        Self { observer, handle }
    }
    pub(super) fn shutdown(&self) {
        self.observer.0.stop.store(true, Ordering::Release);
        self.observer.clear();
        self.observer.signal();
    }
    pub(super) fn joined(&mut self) -> bool {
        if self
            .handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            return false;
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        true
    }
    #[cfg(test)]
    pub(super) fn is_joined(&self) -> bool {
        self.handle.is_none()
    }
}
impl Drop for ObservationWorker {
    fn drop(&mut self) {
        self.shutdown();
        // Final ownership fallback, not the nonblocking UI shutdown path.
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
struct PhysicalNode {
    identity: DeviceIdentity,
    usb: PathBuf,
    node: PathBuf,
}
#[derive(Default)]
struct Inventory {
    target: Option<Arc<RecoveryWatchTarget>>,
    physical: Option<DeviceIdentity>,
    pending_physical: Option<DeviceIdentity>,
    epoch: u64,
    topology_revision: u64,
    last_removal: Option<ObservationEpoch>,
    cached: Vec<PhysicalNode>,
    staged: Vec<PhysicalNode>,
    incumbent: Vec<PhysicalNode>,
    last_snapshot: Option<Result<Arc<CaptureSnapshot>, ApplyFailure>>,
    last_audio: Option<SourcePresence>,
    observed_watch: Option<crate::domain::capture::WatchId>,
    observed_removal: Option<ObservationEpoch>,
    observed_video: Option<std::mem::Discriminant<VideoPresence>>,
    observed_context: Option<SelectionToken>,
    choice_changed: bool,
    force_epoch: bool,
}
impl Inventory {
    fn set_target(&mut self, target: Option<Arc<RecoveryWatchTarget>>) {
        if self.target.as_ref().map(|target| target.watch)
            != target.as_ref().map(|target| target.watch)
        {
            self.physical = target.as_ref().map(|target| target.video.identity.clone());
            self.pending_physical = None;
            self.target = target;
            self.last_removal = None;
            self.incumbent.clear();
            self.staged.clear();
        }
    }
    fn next_epoch(&mut self) -> Result<ObservationEpoch, &'static str> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or("observation epoch exhausted")?;
        ObservationEpoch::new(self.epoch).ok_or("observation epoch exhausted")
    }
    #[cfg(test)]
    fn event(&mut self, event: &TopologyEvent) -> Result<(), &'static str> {
        self.event_selected(event, None)
    }

    fn event_selected(
        &mut self,
        event: &TopologyEvent,
        selected: Option<&SelectedRouteAuthorization>,
    ) -> Result<(), &'static str> {
        self.topology_revision = self
            .topology_revision
            .checked_add(1)
            .ok_or("topology revision exhausted")?;
        let Some(target) = &self.target else {
            return Ok(());
        };
        let path_matches = |node: &PhysicalNode| {
            node.node.starts_with(&event.syspath) || node.usb.starts_with(&event.syspath)
        };
        let nodes = self
            .cached
            .iter()
            .chain(&self.staged)
            .chain(&self.incumbent);
        self.choice_changed |= nodes.clone().any(|node| {
            choice_identity_matches(&target.video.identity, &node.identity) && path_matches(node)
        });
        let selected = selected.filter(|selected| selected.token.stamp.watch == target.watch);
        let matches = if let Some(selected) = selected {
            selected.node_syspath.starts_with(&event.syspath)
                || selected.usb_syspath.starts_with(&event.syspath)
        } else {
            nodes.clone().any(|node| {
                path_matches(node)
                    && (self.physical.as_ref() == Some(&node.identity)
                        || self.pending_physical.as_ref() == Some(&node.identity))
            }) || self.incumbent.iter().any(path_matches)
        };
        if event.removed && matches {
            self.last_removal = Some(self.next_epoch()?);
        }
        Ok(())
    }

    /// Stage newest routes WITHOUT replacing the pre-scan attribution. Both
    /// sets remain available until the queued-event reconciliation completes.
    fn cache_snapshot(&mut self, snapshot: &CaptureSnapshot) {
        self.pending_physical = self.target.as_ref().and_then(|target| {
            resolve_identity(
                &target.video.identity,
                snapshot.devices().iter().map(|device| device.identity()),
            )
            .ok()
            .map(|index| snapshot.devices()[index].identity().clone())
        });
        self.staged = snapshot
            .devices()
            .iter()
            .flat_map(|device| {
                device.nodes().iter().map(|node| PhysicalNode {
                    identity: device.identity().clone(),
                    usb: node.usb_syspath().to_owned(),
                    node: node.syspath().to_owned(),
                })
            })
            .collect();
    }

    fn commit_cache(&mut self) {
        if let Some(physical) = self.pending_physical.take() {
            self.physical = Some(physical);
        }
        if !self.staged.is_empty() {
            self.cached = std::mem::take(&mut self.staged);
        } else {
            self.cached
                .retain(|node| self.physical.as_ref() == Some(&node.identity));
        }
        if self.incumbent.is_empty() {
            self.incumbent = self
                .cached
                .iter()
                .filter(|node| self.physical.as_ref() == Some(&node.identity))
                .cloned()
                .collect();
        }
    }

    #[cfg(test)]
    fn observe(
        &mut self,
        snapshot: Result<CaptureSnapshot, ApplyFailure>,
        catalog: Result<Vec<audio::AudioSource>, AudioError>,
    ) -> Result<FreshScan, &'static str> {
        self.observe_selected(snapshot, catalog, None)
    }

    fn observe_selected(
        &mut self,
        snapshot: Result<CaptureSnapshot, ApplyFailure>,
        catalog: Result<Vec<audio::AudioSource>, AudioError>,
        selected: Option<&SelectedRouteAuthorization>,
    ) -> Result<FreshScan, &'static str> {
        let target = Arc::clone(self.target.as_ref().ok_or("no observation target")?);
        let selected = selected.filter(|selected| selected.token.stamp.watch == target.watch);
        let selected_context = selected.map(|selected| selected.token);
        let snapshot = snapshot.map(Arc::new);
        let audio = match &target.audio {
            AudioSelection::Disabled { .. } => SourcePresence::Disabled,
            AudioSelection::Enabled { source } => match catalog {
                Err(error) => SourcePresence::Unknown(error),
                Ok(catalog) => match audio::validate_snapshot(source, &catalog) {
                    Ok(()) => SourcePresence::Present,
                    Err(
                        error @ (AudioError::SourceMissing { .. }
                        | AudioError::SourceChanged { .. }),
                    ) => SourcePresence::Absent(error),
                    Err(error) => SourcePresence::Unknown(error),
                },
            },
        };
        let snapshot_changed = match (&self.last_snapshot, &snapshot) {
            (Some(Ok(previous)), Ok(current)) => !equivalent_snapshots(&target, previous, current),
            (Some(Err(previous)), Err(current)) => previous != current,
            _ => true,
        };
        let audio_changed = self.last_audio.as_ref() != Some(&audio);
        let mut video = match &snapshot {
            Err(error) => VideoPresence::Unknown(error.clone()),
            Ok(snapshot) => {
                let selected_present = selected
                    .is_some_and(|selected| authorized_route_present(&target, snapshot, selected));
                if selected.is_some() && !selected_present {
                    VideoPresence::Absent
                } else {
                    match resolve_identity(
                        &target.video.identity,
                        snapshot.devices().iter().map(|d| d.identity()),
                    ) {
                        Ok(_) => VideoPresence::Present,
                        Err(IdentityError::NotFound) if selected_present => VideoPresence::Present,
                        Err(IdentityError::NotFound) => VideoPresence::Absent,
                        Err(
                            IdentityError::DuplicateSerial { .. }
                            | IdentityError::SeriallessDuplicates { .. },
                        ) => VideoPresence::Ambiguous(Vec::new()),
                    }
                }
            }
        };
        let video_kind = std::mem::discriminant(&video);
        let changed = snapshot_changed
            || audio_changed
            || self.observed_watch != Some(target.watch)
            || self.observed_removal != self.last_removal
            || self.choice_changed
            || self.force_epoch
            || self.observed_video != Some(video_kind);
        let epoch = if changed {
            self.next_epoch()?
        } else {
            ObservationEpoch::new(self.epoch).ok_or("no completed observation epoch")?
        };
        let stamp = WatchStamp {
            watch: target.watch,
            epoch,
        };
        let mut candidates = Vec::new();
        if let (VideoPresence::Ambiguous(displayed), Ok(snapshot)) = (&mut video, &snapshot) {
            for device in snapshot
                .devices()
                .iter()
                .filter(|device| choice_identity_matches(&target.video.identity, device.identity()))
            {
                let mut routes = device.nodes().iter().filter(|node| {
                    matches!(
                        node.capabilities()
                            .assess(CaptureBufferType::SinglePlanar, target.video.mode),
                        SupportVerdict::Supported | SupportVerdict::NeedsExactIntervals
                    )
                });
                let Some(node) = routes.next() else {
                    continue;
                };
                if routes.next().is_some() {
                    continue;
                }
                let ordinal = u64::try_from(candidates.len())
                    .ok()
                    .and_then(|n| n.checked_add(1))
                    .and_then(CandidateId::new)
                    .ok_or("candidate ordinal exhausted")?;
                let token = SelectionToken {
                    stamp,
                    candidate: ordinal,
                };
                candidates.push(SelectedRouteAuthorization {
                    token,
                    identity: device.identity().clone(),
                    usb_syspath: node.usb_syspath().to_owned(),
                    node_syspath: node.syspath().to_owned(),
                });
                displayed.push(RecoveryCandidate {
                    token,
                    identity: device.identity().clone(),
                    description: node.card_name().to_owned(),
                });
            }
        }
        if snapshot_changed {
            self.last_snapshot = Some(snapshot.clone());
        }
        if audio_changed {
            self.last_audio = Some(audio.clone());
        }
        self.observed_watch = Some(target.watch);
        self.observed_removal = self.last_removal;
        self.observed_video = Some(video_kind);
        self.observed_context = selected_context;
        self.choice_changed = false;
        self.force_epoch = false;
        if snapshot.is_ok() {
            self.commit_cache();
        }
        Ok(FreshScan {
            target,
            observation: RecoveryObservation {
                stamp,
                video,
                audio,
                last_video_removal: self.last_removal,
            },
            snapshot,
            candidates,
            topology_revision: self.topology_revision,
            selected_context,
        })
    }
}

fn authorized_route_present(
    target: &RecoveryWatchTarget,
    snapshot: &CaptureSnapshot,
    selected: &SelectedRouteAuthorization,
) -> bool {
    let mut devices = snapshot.devices().iter().filter(|device| {
        device.identity() == &selected.identity
            && device.nodes().iter().any(|node| {
                node.usb_syspath() == selected.usb_syspath
                    && node.syspath() == selected.node_syspath
            })
    });
    let Some(device) = devices.next() else {
        return false;
    };
    if devices.next().is_some() {
        return false;
    }
    let mut routes = device.nodes().iter().filter(|node| {
        matches!(
            node.capabilities()
                .assess(CaptureBufferType::SinglePlanar, target.video.mode),
            SupportVerdict::Supported | SupportVerdict::NeedsExactIntervals
        )
    });
    let Some(node) = routes.next() else {
        return false;
    };
    routes.next().is_none()
        && node.usb_syspath() == selected.usb_syspath
        && node.syspath() == selected.node_syspath
}

fn equivalent_snapshots(
    target: &RecoveryWatchTarget,
    previous: &CaptureSnapshot,
    current: &CaptureSnapshot,
) -> bool {
    let mut previous = previous
        .devices()
        .iter()
        .filter(|device| choice_identity_matches(&target.video.identity, device.identity()));
    let mut current = current
        .devices()
        .iter()
        .filter(|device| choice_identity_matches(&target.video.identity, device.identity()));
    loop {
        match (previous.next(), current.next()) {
            (None, None) => return true,
            (Some(previous), Some(current))
                if previous.identity() == current.identity()
                    && previous.nodes().len() == current.nodes().len()
                    && previous
                        .nodes()
                        .iter()
                        .zip(current.nodes())
                        .all(|(old, new)| {
                            old.devnode() == new.devnode()
                                && old.syspath() == new.syspath()
                                && old.usb_syspath() == new.usb_syspath()
                                && old.raw_capabilities() == new.raw_capabilities()
                                && old.raw_device_caps() == new.raw_device_caps()
                                && old.effective_capabilities() == new.effective_capabilities()
                                && old.capabilities() == new.capabilities()
                        }) => {}
            _ => return false,
        }
    }
}

fn choice_identity_matches(requested: &DeviceIdentity, observed: &DeviceIdentity) -> bool {
    requested.vendor_id() == observed.vendor_id()
        && requested.product_id() == observed.product_id()
        && requested.serial() == observed.serial()
}
fn settings(target: &RecoveryWatchTarget) -> DraftSettings {
    DraftSettings {
        video: target.video.clone(),
        audio: target.audio.clone(),
    }
}
pub(super) fn observation_failure(
    requested: &DraftSettings,
    detail: impl Into<String>,
) -> ApplyFailure {
    ApplyFailure::new(
        FailureCategory::Validation(ValidationLayer::Discovery),
        Stage::Prevalidation,
        Cause::Generic,
        requested.clone(),
        "observe capture topology",
        detail,
    )
}

fn run(
    observer: Observer,
    mut reader: UnixStream,
    fixture_catalog: Option<Vec<audio::AudioSource>>,
) {
    let fixture = fixture_catalog.is_some();
    let mut monitor = if fixture {
        None
    } else {
        Some(Monitor::subscribe())
    };
    let mut source = if fixture {
        None
    } else {
        Some(SourceInventorySubscription::connect(Arc::clone(
            &observer.0.stop,
        )))
    };
    let mut inventory = Inventory::default();
    let mut revision = 0;
    let mut completed = 0;
    let mut dirty = true;
    while !observer.0.stop.load(Ordering::Acquire) {
        if let Some(wake) = &observer.0.wake {
            wake.drain(&mut reader);
        }
        let (target, target_revision, refresh, refresh_choices, selected) = {
            let slots = observer.0.slots.lock();
            (
                slots.target.clone(),
                slots.target_revision,
                slots.refresh_requested,
                slots.refresh_choices,
                slots.selected_route.clone(),
            )
        };
        let mut retry_subscription = refresh > completed;
        if inventory.target.is_some() != target.is_some() || revision != target_revision {
            inventory.set_target(target);
            revision = target_revision;
            dirty = true;
            retry_subscription = true;
        }
        dirty |= refresh > completed;
        inventory.force_epoch |= refresh > completed && refresh_choices;
        let context = selected
            .as_ref()
            .filter(|selected| {
                inventory
                    .target
                    .as_ref()
                    .is_some_and(|target| target.watch == selected.token.stamp.watch)
            })
            .map(|selected| selected.token);
        dirty |= inventory.observed_context != context;
        let mut topology_failure = None;
        if let Some(Ok(monitor)) = &mut monitor {
            loop {
                if observer.0.stop.load(Ordering::Acquire) {
                    return;
                }
                match monitor.next_event() {
                    Ok(Some(event)) => {
                        dirty = true;
                        retry_subscription = true;
                        let slots = observer.0.slots.lock();
                        if let Err(error) =
                            inventory.event_selected(&event, slots.selected_route.as_deref())
                        {
                            topology_failure = Some(error.to_owned());
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        topology_failure = Some(error.to_string());
                        break;
                    }
                }
            }
        }
        // Re-establish a failed control subscription only for an admitted
        // target/validation or real topology event. No timed open/retry loop.
        if retry_subscription && !fixture {
            if monitor.as_ref().is_some_and(Result::is_err) {
                monitor = Some(Monitor::subscribe());
                dirty = true;
            }
            if source.as_ref().is_some_and(Result::is_err) {
                source = Some(SourceInventorySubscription::connect(Arc::clone(
                    &observer.0.stop,
                )));
                dirty = true;
            }
        }
        if let Some(error) = topology_failure {
            monitor = Some(Err(linux::CaptureError::UdevFailure {
                operation: "udev_monitor_receive",
            }));
            if let Some(target) = inventory.target.clone() {
                publish_failure(
                    &observer,
                    &mut inventory,
                    &settings(&target),
                    error,
                    refresh,
                );
            }
            dirty = false;
            completed = refresh;
        }
        let source_dirty = match &mut source {
            Some(Ok(subscription)) => Some(subscription.poll_dirty()),
            _ => None,
        };
        match source_dirty {
            Some(Ok(changed)) => dirty |= changed,
            Some(Err(error)) => {
                source = Some(Err(error));
                dirty = true;
            }
            None => {}
        }
        if dirty && let Some(target) = inventory.target.clone() {
            let requested = settings(&target);
            let snapshot = match &monitor {
                Some(Err(error)) => Err(super::apply::capture_failure(
                    &requested,
                    "subscribe capture topology",
                    error,
                )),
                _ => {
                    #[cfg(test)]
                    if fixture {
                        Ok(linux::session_fixture(
                            &["/dev/video0"],
                            requested.video.mode,
                        ))
                    } else {
                        discover_snapshot(&requested)
                    }
                    #[cfg(not(test))]
                    discover_snapshot(&requested)
                }
            };
            let catalog = if requested.audio.enabled() {
                match &mut source {
                    Some(Ok(source)) => source.snapshot(),
                    Some(Err(error)) => Err(error.clone()),
                    None => match &fixture_catalog {
                        Some(catalog) => Ok(catalog.clone()),
                        None => Err(AudioError::Unavailable(
                            "source inventory subscription missing".into(),
                        )),
                    },
                }
            } else {
                Ok(Vec::new())
            };
            if let Ok(snapshot) = &snapshot {
                inventory.cache_snapshot(snapshot);
            }
            // Drain events after the scan before publishing. If the scan raced
            // topology, preserve matching removes and rescan; never publish a
            // pre-removal Present as readiness.
            let mut raced = false;
            let mut receive_failure = None;
            if let Some(Ok(monitor)) = &mut monitor {
                loop {
                    if observer.0.stop.load(Ordering::Acquire) {
                        return;
                    }
                    match monitor.next_event() {
                        Ok(Some(event)) => {
                            raced = true;
                            let result = {
                                let slots = observer.0.slots.lock();
                                inventory.event_selected(&event, slots.selected_route.as_deref())
                            };
                            if let Err(error) = result {
                                publish_failure(
                                    &observer,
                                    &mut inventory,
                                    &requested,
                                    error,
                                    refresh,
                                );
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            receive_failure = Some(error);
                            break;
                        }
                    }
                }
            }
            if let Some(error) = receive_failure {
                let detail = error.to_string();
                monitor = Some(Err(error));
                publish_failure(&observer, &mut inventory, &requested, detail, refresh);
                completed = refresh;
                dirty = false;
                continue;
            }
            let source_dirty = match &mut source {
                Some(Ok(subscription)) => Some(subscription.poll_dirty()),
                _ => None,
            };
            match source_dirty {
                Some(Ok(changed)) => raced |= changed,
                Some(Err(error)) => {
                    source = Some(Err(error));
                    raced = true;
                }
                None => {}
            }
            if raced {
                dirty = true;
                continue;
            }
            match inventory.observe_selected(snapshot, catalog, selected.as_deref()) {
                Ok(scan) => publish(&observer, scan, revision, refresh),
                Err(error) => {
                    publish_failure(&observer, &mut inventory, &requested, error, refresh);
                    return;
                }
            }
            completed = refresh;
            dirty = false;
        }
        if let Some(Ok(subscription)) = &monitor {
            if let Err(error) = subscription.wait(reader.as_raw_fd()) {
                monitor = Some(Err(error));
                dirty = true;
            }
        } else {
            // No usable native subscription: await only target/validation/stop
            // notifications, not timer-driven discovery or synthetic health.
            let mut slots = observer.0.slots.lock();
            if !observer.0.stop.load(Ordering::Acquire)
                && slots.refresh_requested <= completed
                && slots.target_revision == revision
                && slots.target.is_some() == inventory.target.is_some()
            {
                observer
                    .0
                    .changed
                    .wait_for(&mut slots, Duration::from_millis(50));
            }
        }
    }
}
fn discover_snapshot(requested: &DraftSettings) -> Result<CaptureSnapshot, ApplyFailure> {
    match linux::discover() {
        Ok(snapshot) => Ok(snapshot),
        Err(linux::CaptureError::NoVideoNodes | linux::CaptureError::NoCaptureNodes) => {
            Ok(linux::empty_snapshot())
        }
        Err(error) => Err(super::apply::capture_failure(
            requested,
            "discover recovery capture devices",
            &error,
        )),
    }
}
fn publish(observer: &Observer, scan: FreshScan, revision: u64, refresh: u64) {
    let mut slots = observer.0.slots.lock();
    if slots.target_revision == revision
        && slots
            .target
            .as_ref()
            .is_some_and(|t| t.watch == scan.observation.stamp.watch)
        && !observer.0.stop.load(Ordering::Acquire)
        && slots
            .selected_route
            .as_ref()
            .filter(|selected| selected.token.stamp.watch == scan.observation.stamp.watch)
            .map(|selected| selected.token)
            == scan.selected_context
    {
        if slots
            .consumed_epoch
            .is_some_and(|epoch| epoch != scan.observation.stamp)
        {
            slots.consumed_epoch = None;
            slots.consumed_candidates.clear();
        }
        slots.latest = Some(scan.observation.clone());
        slots.current = Some(Arc::new(scan));
        slots.refresh_completed = refresh;
        drop(slots);
        observer.0.changed.notify_all();
    }
}
fn publish_failure(
    observer: &Observer,
    inventory: &mut Inventory,
    requested: &DraftSettings,
    detail: impl Into<String>,
    refresh: u64,
) {
    let (revision, selected) = {
        let slots = observer.0.slots.lock();
        (slots.target_revision, slots.selected_route.clone())
    };
    if let Ok(scan) = inventory.observe_selected(
        Err(observation_failure(requested, detail)),
        Err(AudioError::Unavailable("capture observation failed".into())),
        selected.as_deref(),
    ) {
        publish(observer, scan, revision, refresh);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        capture::{
            CaptureMode, CapturedFourCc, FrameRate, FrameSize, ModeRequest, UsbTopology, WatchId,
        },
        state::{ApplyId, AttemptPurpose, DraftRevision, InitialPlayback, ValidationKey},
    };

    fn mode() -> CaptureMode {
        CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(1280, 720).unwrap(),
            rate: FrameRate::new(60000, 1001).unwrap(),
        }
    }
    fn identity(port: u8, serial: Option<&str>) -> DeviceIdentity {
        DeviceIdentity::new(
            0x32ed,
            0x3701,
            UsbTopology::new(
                "pci-fixture".into(),
                vec![std::num::NonZeroU8::new(port).unwrap()],
            )
            .unwrap(),
            serial.map(str::to_owned),
        )
        .unwrap()
    }
    fn target(identity: DeviceIdentity, id: u64) -> RecoveryWatchTarget {
        RecoveryWatchTarget {
            watch: WatchId::new(id).unwrap(),
            video: ModeRequest {
                identity,
                mode: mode(),
            },
            audio: AudioSelection::default(),
        }
    }
    fn inventory(target: RecoveryWatchTarget) -> Inventory {
        let mut inventory = Inventory::default();
        inventory.set_target(Some(Arc::new(target)));
        inventory
    }
    fn snapshot(devices: &[(DeviceIdentity, &str, &str, &str)]) -> CaptureSnapshot {
        linux::physical_fixture(
            &devices
                .iter()
                .map(|(id, dev, usb, node)| (id.clone(), *dev, *usb, *node, mode()))
                .collect::<Vec<_>>(),
        )
    }
    fn scan(inventory: &mut Inventory, snapshot: CaptureSnapshot) -> FreshScan {
        inventory.cache_snapshot(&snapshot);
        inventory.observe(Ok(snapshot), Ok(Vec::new())).unwrap()
    }
    fn scan_for_observer(
        inventory: &mut Inventory,
        snapshot: CaptureSnapshot,
        observer: &Observer,
    ) -> FreshScan {
        let selected = observer.0.slots.lock().selected_route.clone();
        inventory.cache_snapshot(&snapshot);
        inventory
            .observe_selected(Ok(snapshot), Ok(Vec::new()), selected.as_deref())
            .unwrap()
    }
    fn observer() -> Observer {
        Observer(Arc::new(Shared {
            slots: Mutex::new(Slots::default()),
            changed: Condvar::new(),
            stop: Arc::new(AtomicBool::new(false)),
            wake: None,
        }))
    }
    fn request(target: &RecoveryWatchTarget, stamp: WatchStamp) -> ValidationRequest {
        ValidationRequest {
            key: ValidationKey {
                apply: ApplyId::new(1).unwrap(),
                purpose: AttemptPurpose::Recovery,
            },
            revision: DraftRevision::default(),
            settings: settings(target),
            playback: InitialPlayback::Live,
            watch: stamp,
            choice: None,
        }
    }

    #[test]
    fn cached_physical_removal_survives_absence_and_coalesced_replug() {
        let target = target(identity(1, Some("saved")), 1);
        let mut inventory = inventory(target.clone());
        let observer = observer();
        observer.watch(target).unwrap();
        let first = scan(
            &mut inventory,
            snapshot(&[(
                identity(1, Some("saved")),
                "/dev/video0",
                "/sys/usb/1",
                "/sys/usb/1/video0",
            )]),
        );
        assert!(matches!(first.observation.video, VideoPresence::Present));
        publish(&observer, first, 1, 0);
        let absent = scan(&mut inventory, linux::empty_snapshot());
        assert!(matches!(absent.observation.video, VideoPresence::Absent));
        assert!(
            absent.observation.last_video_removal.is_none(),
            "a missing scan is not a physical removal receipt"
        );
        publish(&observer, absent, 1, 0);
        inventory
            .event(&TopologyEvent {
                syspath: "/sys/usb/1".into(),
                removed: true,
            })
            .unwrap();
        let removed = inventory.last_removal.unwrap();
        let returned = scan(
            &mut inventory,
            snapshot(&[(
                identity(1, Some("saved")),
                "/dev/video9",
                "/sys/usb/1",
                "/sys/usb/1/video9",
            )]),
        );
        publish(&observer, returned, 1, 0);
        let latest = observer.poll().unwrap();
        assert!(matches!(latest.video, VideoPresence::Present));
        assert_eq!(
            latest.last_video_removal,
            Some(removed),
            "the one latest slot must not erase loss when replug replaces absence"
        );
        assert!(latest.stamp.epoch.get() > removed.get());
        assert!(observer.poll().is_none());
    }

    #[test]
    fn foreign_duplicate_serial_or_same_label_remove_does_not_prove_saved_card_removed() {
        let mut inventory = inventory(target(identity(1, Some("same")), 1));
        scan(
            &mut inventory,
            snapshot(&[
                (
                    identity(1, Some("same")),
                    "/dev/video0",
                    "/sys/usb/1",
                    "/sys/usb/1/video0",
                ),
                (
                    identity(2, Some("same")),
                    "/dev/video1",
                    "/sys/usb/2",
                    "/sys/usb/2/video1",
                ),
            ]),
        );
        inventory
            .event(&TopologyEvent {
                syspath: "/sys/usb/2".into(),
                removed: true,
            })
            .unwrap();
        assert!(inventory.last_removal.is_none());
        inventory
            .event(&TopologyEvent {
                syspath: "/dev/video0".into(),
                removed: true,
            })
            .unwrap();
        assert!(
            inventory.last_removal.is_none(),
            "a node name is not physical attribution"
        );
        inventory
            .event(&TopologyEvent {
                syspath: "/sys/usb/1/video0".into(),
                removed: true,
            })
            .unwrap();
        assert!(inventory.last_removal.is_some());
    }

    #[test]
    fn uniquely_relocated_serial_caches_actual_physical_path() {
        let mut inventory = inventory(target(identity(1, Some("saved")), 1));
        scan(
            &mut inventory,
            snapshot(&[(
                identity(7, Some("saved")),
                "/dev/video8",
                "/sys/usb/7",
                "/sys/usb/7/video8",
            )]),
        );
        inventory
            .event(&TopologyEvent {
                syspath: "/sys/usb/7".into(),
                removed: true,
            })
            .unwrap();
        assert!(inventory.last_removal.is_some());
    }

    #[test]
    fn failed_scan_and_failed_source_subscription_are_unknown_not_healthy_or_absent() {
        let mut target = target(identity(1, None), 1);
        target.audio = AudioSelection::Enabled {
            source: crate::domain::capture::AudioSourceIdentity::new("saved-source".into(), vec![])
                .unwrap(),
        };
        let mut inventory = inventory(target.clone());
        let scan = inventory
            .observe(
                Err(observation_failure(&settings(&target), "ENOBUFS")),
                Err(AudioError::Unavailable("subscription disconnected".into())),
            )
            .unwrap();
        assert!(matches!(scan.observation.video, VideoPresence::Unknown(_)));
        assert!(matches!(scan.observation.audio, SourcePresence::Unknown(_)));
        assert!(scan.observation.last_video_removal.is_none());
    }

    #[test]
    fn source_observation_never_substitutes_available_microphone_for_saved_identity() {
        let mut target = target(identity(1, None), 1);
        let saved = crate::domain::capture::AudioSourceIdentity::new(
            "saved-source".into(),
            vec![("card.name".into(), "capture".into())],
        )
        .unwrap();
        target.audio = AudioSelection::Enabled {
            source: saved.clone(),
        };
        let mut inventory = inventory(target);
        let video = snapshot(&[(
            identity(1, None),
            "/dev/video0",
            "/sys/usb/1",
            "/sys/usb/1/video0",
        )]);
        inventory.cache_snapshot(&video);
        let mic = audio::AudioSource {
            identity: crate::domain::capture::AudioSourceIdentity::new("microphone".into(), vec![])
                .unwrap(),
            description: "capture".into(),
        };
        let absent = inventory.observe(Ok(video.clone()), Ok(vec![mic])).unwrap();
        assert!(matches!(
            absent.observation.audio,
            SourcePresence::Absent(AudioError::SourceMissing { .. })
        ));
        let present = inventory
            .observe(
                Ok(video),
                Ok(vec![audio::AudioSource {
                    identity: saved,
                    description: "renamed human label".into(),
                }]),
            )
            .unwrap();
        assert_eq!(present.observation.audio, SourcePresence::Present);
    }

    #[test]
    fn ambiguity_tokens_name_actual_physical_candidates_and_expire_after_new_scan() {
        let target = target(identity(1, None), 1);
        let observer = observer();
        observer.watch(target.clone()).unwrap();
        let mut inventory = inventory(target.clone());
        let devices = [
            (
                identity(1, None),
                "/dev/video0",
                "/sys/usb/1",
                "/sys/usb/1/video0",
            ),
            (
                identity(2, None),
                "/dev/video1",
                "/sys/usb/2",
                "/sys/usb/2/video1",
            ),
            (
                identity(3, Some("foreign")),
                "/dev/video2",
                "/sys/usb/3",
                "/sys/usb/3/video2",
            ),
        ];
        let first = scan(&mut inventory, snapshot(&devices));
        let token = first.candidates[1].token;
        assert_eq!(first.candidates.len(), 2);
        assert_eq!(first.candidates[1].identity, identity(2, None));
        assert_eq!(first.candidates[1].usb_syspath, PathBuf::from("/sys/usb/2"));
        publish(&observer, first, 1, 0);
        let unchanged = scan(&mut inventory, snapshot(&devices));
        assert_eq!(
            unchanged.candidates[1].token, token,
            "fresh unchanged scan does not invent topology"
        );
        inventory
            .event(&TopologyEvent {
                syspath: "/sys/usb/2".into(),
                removed: true,
            })
            .unwrap();
        let refreshed = scan(&mut inventory, snapshot(&devices));
        assert_ne!(refreshed.candidates[1].token, token);
        publish(&observer, refreshed, 1, 0);
        let mut request = request(&target, token.stamp);
        request.choice = Some(token);
        let failure = observer
            .fresh(&request)
            .err()
            .expect("old selection cannot be admitted");
        assert!(failure.diagnostic.contains("stale"));
    }

    #[test]
    fn accepted_choice_is_one_use_and_fresh_removal_invalidates_it() {
        let target = target(identity(1, None), 1);
        let observer = observer();
        observer.watch(target.clone()).unwrap();
        let mut inventory = inventory(target.clone());
        let devices = [
            (
                identity(1, None),
                "/dev/video0",
                "/sys/usb/1",
                "/sys/usb/1/video0",
            ),
            (
                identity(2, None),
                "/dev/video1",
                "/sys/usb/2",
                "/sys/usb/2/video1",
            ),
        ];
        let first = scan(&mut inventory, snapshot(&devices));
        let token = first.candidates[1].token;
        publish(&observer, first, 1, 0);
        let mut selected = request(&target, token.stamp);
        selected.choice = Some(token);
        let worker = observer.clone();
        let selected_copy = selected.clone();
        let handle = std::thread::spawn(move || worker.fresh(&selected_copy));
        let mut slots = observer.0.slots.lock();
        while slots.refresh_requested == 0 {
            observer.0.changed.wait(&mut slots);
        }
        assert_eq!(slots.consumed_choice, Some(token));
        drop(slots);
        assert!(
            observer.fresh(&selected).is_err(),
            "one-use token cannot enter a second scan"
        );
        inventory
            .event(&TopologyEvent {
                syspath: "/sys/usb/2".into(),
                removed: true,
            })
            .unwrap();
        let replug = scan_for_observer(&mut inventory, snapshot(&devices), &observer);
        publish(&observer, replug, 1, 1);
        let failure = handle
            .join()
            .unwrap()
            .err()
            .expect("coalesced replug must not save a removed choice");
        assert!(failure.diagnostic.contains("topology"));
    }

    #[test]
    fn replaced_or_cleared_watch_rejects_late_observations_and_old_target_ids() {
        let observer = observer();
        let first_target = target(identity(1, None), 1);
        observer.watch(first_target.clone()).unwrap();
        let mut inventory = inventory(first_target.clone());
        let old = scan(&mut inventory, linux::empty_snapshot());
        let old_stamp = old.observation.stamp;
        observer.watch(target(identity(2, None), 2)).unwrap();
        publish(&observer, old, 1, 0);
        assert!(observer.poll().is_none());
        assert!(observer.fresh(&request(&first_target, old_stamp)).is_err());
        observer.clear();
        assert_eq!(
            observer.watch(first_target),
            Err(SubmitFailure::Disconnected)
        );
    }

    #[test]
    fn observer_shutdown_wakes_waiters_and_reaps_actual_worker() {
        let mut worker = ObservationWorker::fixture();
        let observer = observer();
        let target = target(identity(1, Some("fixture")), 1);
        observer.watch(target.clone()).unwrap();
        let waiting = observer.clone();
        let handle = std::thread::spawn(move || {
            waiting.fresh(&request(
                &target,
                WatchStamp {
                    watch: target.watch,
                    epoch: ObservationEpoch::new(1).unwrap(),
                },
            ))
        });
        let mut slots = observer.0.slots.lock();
        while slots.refresh_requested == 0 {
            observer.0.changed.wait(&mut slots);
        }
        observer.0.stop.store(true, Ordering::Release);
        drop(slots);
        observer.signal();
        worker.shutdown();
        assert!(handle.join().unwrap().is_err());
        for _ in 0..2000 {
            if worker.joined() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            worker.handle.is_none(),
            "join must actually consume the worker handle"
        );
        assert!(worker.observer.0.slots.lock().finished);
    }

    #[test]
    fn epoch_exhaustion_never_wraps_or_reuses_selection_tokens() {
        let mut inventory = inventory(target(identity(1, None), 1));
        inventory.epoch = u64::MAX;
        assert!(inventory.next_epoch().is_err());
        assert_eq!(inventory.epoch, u64::MAX);
    }

    #[test]
    fn queued_remove_of_pre_scan_serial_location_survives_relocated_snapshot() {
        let mut inventory = inventory(target(identity(1, Some("saved")), 1));
        let first = scan(
            &mut inventory,
            snapshot(&[(
                identity(1, Some("saved")),
                "/dev/video0",
                "/sys/usb/1",
                "/sys/usb/1/video0",
            )]),
        );
        let moved = snapshot(&[(
            identity(2, Some("saved")),
            "/dev/video9",
            "/sys/usb/2",
            "/sys/usb/2/video9",
        )]);
        inventory.cache_snapshot(&moved);
        assert_eq!(
            inventory.physical,
            Some(identity(1, Some("saved"))),
            "staging B cannot discard pre-scan attribution A"
        );
        inventory
            .event(&TopologyEvent {
                syspath: "/sys/usb/1".into(),
                removed: true,
            })
            .unwrap();
        let removed = inventory
            .last_removal
            .expect("queued remove A must remain proven");
        assert!(removed.get() > first.observation.stamp.epoch.get());
        let returned = inventory.observe(Ok(moved), Ok(Vec::new())).unwrap();
        assert_eq!(returned.observation.video, VideoPresence::Present);
        assert_eq!(returned.observation.last_video_removal, Some(removed));
        assert_eq!(inventory.physical, Some(identity(2, Some("saved"))));
        let repeated = scan(
            &mut inventory,
            snapshot(&[(
                identity(2, Some("saved")),
                "/dev/video9",
                "/sys/usb/2",
                "/sys/usb/2/video9",
            )]),
        );
        assert_eq!(repeated.observation.stamp, returned.observation.stamp);
        assert_eq!(repeated.observation.last_video_removal, Some(removed));
    }

    #[test]
    fn unchanged_validation_scan_keeps_epoch_but_actual_target_tuple_change_advances_it() {
        let mut inventory = inventory(target(identity(1, Some("saved")), 1));
        let devices = [(
            identity(1, Some("saved")),
            "/dev/video0",
            "/sys/usb/1",
            "/sys/usb/1/video0",
        )];
        let first = scan(&mut inventory, snapshot(&devices));
        let second = scan(&mut inventory, snapshot(&devices));
        assert_eq!(
            first.observation.stamp, second.observation.stamp,
            "a fresh request does not manufacture materially-new topology for retries"
        );
        let mut changed = mode();
        changed.rate = FrameRate::new(60, 1).unwrap();
        let changed_snapshot = linux::physical_fixture(&[(
            identity(1, Some("saved")),
            "/dev/video0",
            "/sys/usb/1",
            "/sys/usb/1/video0",
            changed,
        )]);
        inventory.cache_snapshot(&changed_snapshot);
        let changed = inventory
            .observe(Ok(changed_snapshot), Ok(Vec::new()))
            .unwrap();
        assert!(changed.observation.stamp.epoch.get() > second.observation.stamp.epoch.get());
    }

    #[test]
    fn admitted_selected_route_removal_is_retained_during_ambiguous_opening() {
        let target = target(identity(1, None), 1);
        let observer = observer();
        observer.watch(target.clone()).unwrap();
        let mut inventory = inventory(target.clone());
        let devices = [
            (
                identity(1, None),
                "/dev/video0",
                "/sys/usb/1",
                "/sys/usb/1/video0",
            ),
            (
                identity(2, None),
                "/dev/video1",
                "/sys/usb/2",
                "/sys/usb/2/video1",
            ),
        ];
        let first = scan(&mut inventory, snapshot(&devices));
        let token = first.candidates[1].token;
        publish(&observer, first, 1, 0);
        let mut selected = request(&target, token.stamp);
        selected.choice = Some(token);
        let validation = observer.clone();
        let handle = std::thread::spawn(move || validation.fresh(&selected));
        let mut slots = observer.0.slots.lock();
        while slots.refresh_requested == 0 {
            observer.0.changed.wait(&mut slots);
        }
        assert_eq!(
            slots.selected_route.as_ref().map(|route| &route.identity),
            Some(&identity(2, None))
        );
        drop(slots);
        let fresh = scan_for_observer(&mut inventory, snapshot(&devices), &observer);
        publish(&observer, fresh, 1, 1);
        let (validated, authorization) = handle.join().unwrap().unwrap();
        assert!(matches!(
            validated.observation.video,
            VideoPresence::Ambiguous(_)
        ));
        assert_eq!(authorization.as_ref().unwrap().identity, identity(2, None));
        // Prepared result has already drained, and owner opening still has the
        // insufficient saved target. Its admitted physical route is retained.
        {
            let slots = observer.0.slots.lock();
            inventory
                .event_selected(
                    &TopologyEvent {
                        syspath: "/sys/usb/2".into(),
                        removed: true,
                    },
                    slots.selected_route.as_deref(),
                )
                .unwrap();
        }
        let removed = inventory
            .last_removal
            .expect("chosen opening route removal must be retained");
        let latest = scan_for_observer(&mut inventory, snapshot(&devices), &observer);
        assert!(matches!(
            latest.observation.video,
            VideoPresence::Ambiguous(_)
        ));
        assert_eq!(latest.observation.last_video_removal, Some(removed));
        assert!(removed.get() > validated.observation.stamp.epoch.get());
        observer.retire_choice(token);
        assert!(observer.0.slots.lock().selected_route.is_none());
    }

    fn open_model_from_scan(
        model: &mut crate::domain::state::ProductModel,
        scan: &FreshScan,
        selected: Option<SelectedRouteAuthorization>,
    ) -> crate::domain::state::AttemptKey {
        let request = model
            .refresh_validation_stamp(scan.observation.stamp)
            .unwrap();
        let snapshot = scan.snapshot.as_ref().unwrap();
        let prepared = match selected {
            None => super::super::apply::validate_prepared(
                request.settings.clone(),
                snapshot,
                &[],
                scan.observation.stamp,
            )
            .unwrap(),
            Some(selected) => super::super::apply::fixture_authorized_prepared_at(
                request.settings.clone(),
                snapshot,
                scan.observation.stamp,
                selected,
            )
            .unwrap(),
        };
        let request = model
            .accept_prepared(&request, prepared.settings().clone(), prepared.stamp())
            .unwrap();
        let Some(crate::domain::state::ModelEffect::Open { key, .. }) =
            model.validation_succeeded(&request)
        else {
            panic!("proven capture evidence must start one opening");
        };
        key
    }

    fn active_model(
        identity: DeviceIdentity,
        devices: &[(DeviceIdentity, &str, &str, &str)],
    ) -> (
        crate::domain::state::ProductModel,
        Inventory,
        crate::domain::state::AttemptKey,
    ) {
        use crate::domain::state::{ModelEffect, ProductModel};
        let settings = DraftSettings {
            video: ModeRequest {
                identity,
                mode: mode(),
            },
            audio: AudioSelection::default(),
        };
        let mut model = ProductModel::new(settings);
        let (_, effect) = model
            .apply(model.state_identity(), model.draft().revision)
            .unwrap();
        assert!(matches!(effect, ModelEffect::Validate(_)));
        let mut inventory = inventory(model.watch_target().unwrap().clone());
        let first = scan(&mut inventory, snapshot(devices));
        model.recovery_observed(&first.observation);
        let key = open_model_from_scan(&mut model, &first, None);
        model.open_verified(key);
        assert!(model.active().is_some());
        (model, inventory, key)
    }

    fn session_failure(settings: &DraftSettings, stage: Stage) -> ApplyFailure {
        ApplyFailure::new(
            FailureCategory::Session,
            stage,
            Cause::Busy,
            settings.clone(),
            "fixture capture session",
            "deterministic terminal session failure",
        )
    }

    #[test]
    fn watcher_unchanged_fresh_scan_does_not_restart_failed_model_recovery() {
        use crate::domain::{capture::LossEvidence, state::ModelEffect};
        let devices = [(
            identity(1, Some("saved")),
            "/dev/video0",
            "/sys/usb/1",
            "/sys/usb/1/video0",
        )];
        let (mut model, mut inventory, active) = active_model(identity(1, Some("saved")), &devices);
        let applied = model.active().unwrap().applied().settings().clone();
        assert!(matches!(
            model.video_lost(
                active.attempt,
                LossEvidence::StreamEnded {
                    reason: 0,
                    error: 0
                },
                session_failure(&applied, Stage::StreamStart)
            ),
            Some(ModelEffect::Stop { .. })
        ));
        // Pure reducer fixture acknowledgement, not a claim of native teardown.
        assert!(model.barrier_complete(active.attempt).is_none());
        assert!(matches!(
            model.continue_recovery(),
            Some(ModelEffect::Validate(_))
        ));
        let fresh = scan(&mut inventory, snapshot(&devices));
        assert!(model.recovery_observed(&fresh.observation).is_none());
        let recovery = open_model_from_scan(&mut model, &fresh, None);
        let requested = model.opening().unwrap().1.clone();
        assert!(matches!(
            model.open_failed(recovery, session_failure(&requested, Stage::Open)),
            Some(ModelEffect::Stop { .. })
        ));
        assert!(model.barrier_complete(recovery.attempt).is_none());
        let repeated = scan(&mut inventory, snapshot(&devices));
        assert_eq!(repeated.observation.stamp, fresh.observation.stamp);
        assert!(model.recovery_observed(&repeated.observation).is_none());
        assert!(
            model.continue_recovery().is_none(),
            "unchanged subscribed scan cannot trigger an automatic open loop"
        );
        assert!(model.opening().is_none());
        assert!(model.validation_request().is_none());
    }

    #[test]
    fn watcher_relocated_scan_queued_removal_stops_actual_model_incumbent_once() {
        use crate::domain::state::ModelEffect;
        let initial = [(
            identity(1, Some("saved")),
            "/dev/video0",
            "/sys/usb/1",
            "/sys/usb/1/video0",
        )];
        let (mut model, mut inventory, active) = active_model(identity(1, Some("saved")), &initial);
        let moved = snapshot(&[(
            identity(2, Some("saved")),
            "/dev/video9",
            "/sys/usb/2",
            "/sys/usb/2/video9",
        )]);
        inventory.cache_snapshot(&moved);
        inventory
            .event(&TopologyEvent {
                syspath: "/sys/usb/1".into(),
                removed: true,
            })
            .unwrap();
        let latest = inventory.observe(Ok(moved), Ok(Vec::new())).unwrap();
        assert!(matches!(latest.observation.video, VideoPresence::Present));
        let Some(ModelEffect::Stop { attempt, .. }) = model.recovery_observed(&latest.observation)
        else {
            panic!("Present plus retained remove A must invalidate the old owner");
        };
        assert_eq!(attempt, active.attempt);
        assert!(model.active().is_none());
        assert!(
            model.recovery_observed(&latest.observation).is_none(),
            "one receipt cannot emit a duplicate stop"
        );
    }

    #[test]
    fn watcher_selected_route_loss_stops_model_opening_despite_ambiguous_inventory() {
        use crate::domain::{capture::LossEvidence, state::ModelEffect};
        let initial = [(
            identity(1, None),
            "/dev/video0",
            "/sys/usb/1",
            "/sys/usb/1/video0",
        )];
        let (mut model, mut inventory, active) = active_model(identity(1, None), &initial);
        let applied = model.active().unwrap().applied().settings().clone();
        model.video_lost(
            active.attempt,
            LossEvidence::StreamEnded {
                reason: 0,
                error: 0,
            },
            session_failure(&applied, Stage::StreamStart),
        );
        let devices = [
            (
                identity(1, None),
                "/dev/video0",
                "/sys/usb/1",
                "/sys/usb/1/video0",
            ),
            (
                identity(2, None),
                "/dev/video1",
                "/sys/usb/2",
                "/sys/usb/2/video1",
            ),
        ];
        let ambiguous = scan(&mut inventory, snapshot(&devices));
        model.recovery_observed(&ambiguous.observation);
        model.barrier_complete(active.attempt);
        model.continue_recovery();
        let VideoPresence::Ambiguous(candidates) = &ambiguous.observation.video else {
            panic!("physical duplicates");
        };
        let token = candidates[1].token;
        assert!(matches!(
            model
                .choose_recovery(model.state_identity(), token)
                .unwrap(),
            Some(ModelEffect::Validate(_))
        ));
        let observer = observer();
        observer
            .watch(model.watch_target().unwrap().clone())
            .unwrap();
        publish(&observer, ambiguous, 1, 0);
        let request = model.validation_request().unwrap().clone();
        let validation = observer.clone();
        let handle = std::thread::spawn(move || validation.fresh(&request));
        let mut slots = observer.0.slots.lock();
        while slots.refresh_requested == 0 {
            observer.0.changed.wait(&mut slots);
        }
        drop(slots);
        let fresh = scan_for_observer(&mut inventory, snapshot(&devices), &observer);
        publish(&observer, fresh, 1, 1);
        let (fresh, authorization) = handle.join().unwrap().unwrap();
        assert!(model.recovery_observed(&fresh.observation).is_none());
        let opening = open_model_from_scan(&mut model, &fresh, authorization);
        let repeated = scan_for_observer(&mut inventory, snapshot(&devices), &observer);
        assert!(model.recovery_observed(&repeated.observation).is_none());
        assert_eq!(model.opening().map(|(key, _)| key), Some(opening));
        // Removal of saved A is not removal of the admitted opening route B.
        {
            let slots = observer.0.slots.lock();
            inventory
                .event_selected(
                    &TopologyEvent {
                        syspath: "/sys/usb/1".into(),
                        removed: true,
                    },
                    slots.selected_route.as_deref(),
                )
                .unwrap();
        }
        assert!(
            inventory.last_removal.is_none(),
            "removal attribution is exclusive to admitted B"
        );
        let only_selected = [(
            identity(2, None),
            "/dev/video1",
            "/sys/usb/2",
            "/sys/usb/2/video1",
        )];
        let selected_present =
            scan_for_observer(&mut inventory, snapshot(&only_selected), &observer);
        assert_eq!(selected_present.observation.video, VideoPresence::Present);
        assert!(
            model
                .recovery_observed(&selected_present.observation)
                .is_none()
        );
        assert_eq!(model.opening().map(|(key, _)| key), Some(opening));
        let both_return = scan_for_observer(&mut inventory, snapshot(&devices), &observer);
        assert!(matches!(
            both_return.observation.video,
            VideoPresence::Ambiguous(_)
        ));
        assert!(model.recovery_observed(&both_return.observation).is_none());
        assert_eq!(model.opening().map(|(key, _)| key), Some(opening));
        {
            let slots = observer.0.slots.lock();
            inventory
                .event_selected(
                    &TopologyEvent {
                        syspath: "/sys/usb/2".into(),
                        removed: true,
                    },
                    slots.selected_route.as_deref(),
                )
                .unwrap();
        }
        let replug = scan_for_observer(&mut inventory, snapshot(&devices), &observer);
        assert!(matches!(
            replug.observation.video,
            VideoPresence::Ambiguous(_)
        ));
        let Some(ModelEffect::Stop { attempt, .. }) = model.recovery_observed(&replug.observation)
        else {
            panic!("the selected opening's physical removal must dominate latest ambiguity");
        };
        assert_eq!(attempt, opening.attempt);
        assert!(model.opening().is_none());
    }

    #[test]
    fn scan_under_retired_grant_cannot_publish_and_automatic_resolution_returns() {
        let target = target(identity(1, None), 1);
        let observer = observer();
        observer.watch(target.clone()).unwrap();
        let mut inventory = inventory(target);
        let devices = [
            (
                identity(1, None),
                "/dev/video0",
                "/sys/usb/1",
                "/sys/usb/1/video0",
            ),
            (
                identity(2, None),
                "/dev/video1",
                "/sys/usb/2",
                "/sys/usb/2/video1",
            ),
        ];
        let first = scan(&mut inventory, snapshot(&devices));
        let grant = first.candidates[1].clone();
        let token = grant.token;
        publish(&observer, first, 1, 0);
        observer.poll().unwrap();
        {
            let mut slots = observer.0.slots.lock();
            slots.consume_choice(token);
            slots.selected_route = Some(Arc::new(grant));
        }
        let only_selected = [(
            identity(2, None),
            "/dev/video1",
            "/sys/usb/2",
            "/sys/usb/2/video1",
        )];
        let stale = scan_for_observer(&mut inventory, snapshot(&only_selected), &observer);
        assert_eq!(stale.observation.video, VideoPresence::Present);
        observer.retire_choice(token);
        publish(&observer, stale, 1, 1);
        assert!(
            observer.poll().is_none(),
            "retired-context discovery cannot expose stale selected Present"
        );
        let automatic = scan_for_observer(&mut inventory, snapshot(&only_selected), &observer);
        assert_eq!(
            automatic.observation.video,
            VideoPresence::Absent,
            "retiring the one-use grant restores strict serial-less topology matching"
        );
        let stamp = automatic.observation.stamp;
        publish(&observer, automatic, 1, 1);
        assert_eq!(observer.poll().unwrap().video, VideoPresence::Absent);
        let repeated = scan_for_observer(&mut inventory, snapshot(&only_selected), &observer);
        assert_eq!(
            repeated.observation.stamp, stamp,
            "retirement cannot cause an epoch retry loop"
        );
    }

    fn admit_fixture_choice(
        observer: &Observer,
        inventory: &mut Inventory,
        target: &RecoveryWatchTarget,
        devices: &[(DeviceIdentity, &str, &str, &str)],
        token: SelectionToken,
    ) {
        let previous = observer.0.slots.lock().refresh_requested;
        let mut selected = request(target, token.stamp);
        selected.choice = Some(token);
        let validation = observer.clone();
        let handle = std::thread::spawn(move || validation.fresh(&selected));
        let mut slots = observer.0.slots.lock();
        while slots.refresh_requested == previous {
            observer.0.changed.wait(&mut slots);
        }
        let refresh = slots.refresh_requested;
        drop(slots);
        let fresh = scan_for_observer(inventory, snapshot(devices), observer);
        publish(observer, fresh, 1, refresh);
        assert_eq!(handle.join().unwrap().unwrap().1.unwrap().token, token);
    }

    #[test]
    fn same_epoch_g_h_g_choice_replay_is_rejected_without_blocking_unconsumed_h() {
        let target = target(identity(1, None), 1);
        let observer = observer();
        observer.watch(target.clone()).unwrap();
        let mut inventory = inventory(target.clone());
        let devices = [
            (
                identity(1, None),
                "/dev/video0",
                "/sys/usb/1",
                "/sys/usb/1/video0",
            ),
            (
                identity(2, None),
                "/dev/video1",
                "/sys/usb/2",
                "/sys/usb/2/video1",
            ),
        ];
        let first = scan(&mut inventory, snapshot(&devices));
        let g = first.candidates[0].token;
        let h = first.candidates[1].token;
        publish(&observer, first, 1, 0);
        admit_fixture_choice(&observer, &mut inventory, &target, &devices, g);
        observer.retire_choice(g);
        let ungranted = scan_for_observer(&mut inventory, snapshot(&devices), &observer);
        assert_eq!(ungranted.observation.stamp, g.stamp);
        publish(&observer, ungranted, 1, 1);
        admit_fixture_choice(&observer, &mut inventory, &target, &devices, h);
        observer.retire_choice(h);
        let ungranted = scan_for_observer(&mut inventory, snapshot(&devices), &observer);
        assert_eq!(ungranted.observation.stamp, g.stamp);
        publish(&observer, ungranted, 1, 2);
        let mut replay = request(&target, g.stamp);
        replay.choice = Some(g);
        assert!(
            observer.fresh(&replay).is_err(),
            "G cannot regain authority after H in the same epoch"
        );
        assert_eq!(
            observer.0.slots.lock().consumed_candidates.len(),
            2,
            "consumption remains bounded by actual offered candidates, not event history"
        );
    }
}
