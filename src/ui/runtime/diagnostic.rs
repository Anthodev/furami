//! Cold report assembly and metadata companions, never transition authority.
use super::*;
use crate::diagnostics::report as dto;
use crate::domain::state::{AppliedSettings, ApplyAdmission, AttemptKey, AttemptPurpose, Draft};

type RetainedSettings = Result<DraftSettings, dto::ProjectionOversized>;
type LoadedOrigin = Option<(ProfileBinding, dto::ProfileOrigin)>;
type ActiveProfileKey = (AttemptId, Option<crate::domain::state::TreatmentAttemptKey>);

/// Count owned storage and work before any private clone or equality. Collection
/// counts are charged before traversal, and strings are charged by O(1) byte len.
fn preflight_settings(
    settings: &DraftSettings,
    limit: usize,
) -> Result<(), dto::ProjectionOversized> {
    let mut budget = dto::ProjectionBudget::new(limit);
    budget.charge(std::mem::size_of::<DraftSettings>())?;
    let identity = &settings.video.identity;
    budget.charge(identity.topology().controller().len())?;
    budget.collection(
        identity.topology().ports().len(),
        std::mem::size_of::<std::num::NonZeroU8>(),
    )?;
    if let Some(serial) = identity.serial() {
        budget.charge(serial.len())?;
    }
    if let Some(path) = &settings.video.selected_node {
        budget.charge(path.as_os_str().as_encoded_bytes().len())?;
    }
    if let Some(source) = settings.audio.source() {
        budget.charge(source.name().len())?;
        let properties = source.stable_properties();
        budget.collection(properties.len(), std::mem::size_of::<(String, String)>())?;
        for (key, value) in properties {
            budget.charge(key.len())?;
            budget.charge(value.len())?;
        }
    }
    Ok(())
}
fn retain_settings(settings: &DraftSettings, limit: usize) -> RetainedSettings {
    preflight_settings(settings, limit)?;
    Ok(settings.clone())
}
fn comparable<'a>(
    retained: &'a RetainedSettings,
    observed: &DraftSettings,
    limit: usize,
) -> Result<&'a DraftSettings, dto::ProjectionOversized> {
    let retained = retained.as_ref().map_err(|error| *error)?;
    preflight_settings(retained, limit)?;
    preflight_settings(observed, limit)?;
    Ok(retained)
}
/// Call only after both complete operands passed preflight.
fn same_source(
    binding: ProfileBinding,
    submitted: &DraftSettings,
    observed: &DraftSettings,
) -> bool {
    let expected = &submitted.video.identity;
    let actual = &observed.video.identity;
    expected == actual
        || (binding == ProfileBinding::Serial
            && expected.serial().is_some()
            && expected.serial() == actual.serial()
            && expected.vendor_id() == actual.vendor_id()
            && expected.product_id() == actual.product_id())
}
fn same_configuration(
    binding: ProfileBinding,
    retained: &RetainedSettings,
    observed: &DraftSettings,
    limit: usize,
) -> Result<bool, dto::ProjectionOversized> {
    let expected = comparable(retained, observed, limit)?;
    Ok(same_source(binding, expected, observed)
        && expected.video.mode == observed.video.mode
        && expected.audio == observed.audio
        && expected.renderer == observed.renderer
        && observed.video.resolved_mode().is_some())
}
#[derive(Clone)]
pub(super) struct ProfileSeed {
    loaded: RetainedSettings,
    binding: ProfileBinding,
    origin: dto::ProfileOrigin,
}
impl ProfileSeed {
    pub(super) fn new(
        loaded: &DraftSettings,
        binding: ProfileBinding,
        origin: dto::ProfileOrigin,
        limit: usize,
    ) -> Self {
        Self {
            loaded: retain_settings(loaded, limit),
            binding,
            origin,
        }
    }
    fn project(
        &self,
        submitted: &RetainedSettings,
        limit: usize,
    ) -> (LoadedOrigin, Result<bool, dto::ProjectionOversized>) {
        let origin = Some((self.binding, self.origin));
        let Ok(submitted) = submitted else {
            return (origin, Err(dto::ProjectionOversized));
        };
        let loaded = match comparable(&self.loaded, submitted, limit) {
            Ok(value) => value,
            Err(error) => return (origin, Err(error)),
        };
        if !same_source(self.binding, loaded, submitted) {
            return (None, Ok(false));
        }
        (origin, Ok(loaded.same_request(submitted)))
    }
}
struct PendingProfile {
    admission: ApplyAdmission,
    revision: DraftRevision,
    submitted: RetainedSettings,
    binding: ProfileBinding,
    opening: Option<AttemptKey>,
    origin: LoadedOrigin,
    matches_loaded: Result<bool, dto::ProjectionOversized>,
}
#[derive(Clone)]
struct VerifiedProfile {
    settings: RetainedSettings,
    binding: ProfileBinding,
    origin: LoadedOrigin,
    matches_loaded: Result<bool, dto::ProjectionOversized>,
}
impl VerifiedProfile {
    fn matches(
        &self,
        observed: &DraftSettings,
        limit: usize,
    ) -> Result<bool, dto::ProjectionOversized> {
        same_configuration(self.binding, &self.settings, observed, limit)
    }
    fn unknown(&self) -> Self {
        Self {
            settings: Err(dto::ProjectionOversized),
            binding: self.binding,
            origin: self.origin,
            matches_loaded: Err(dto::ProjectionOversized),
        }
    }
    fn applied(&self) -> Result<Option<dto::AppliedProfile>, dto::ProjectionOversized> {
        self.settings.as_ref().map_err(|error| *error)?;
        let matches_loaded_settings = self.matches_loaded?;
        Ok(self.origin.map(|(binding, origin)| dto::AppliedProfile {
            binding_kind: binding.into(),
            origin,
            matches_loaded_settings,
        }))
    }
}
#[derive(Default)]
pub(super) struct ProfileProvenance {
    pub(super) selection_seed: Option<ProfileSeed>,
    pending: Option<PendingProfile>,
    active: Option<VerifiedProfile>,
    last_valid: Option<VerifiedProfile>,
    last_active_key: Option<ActiveProfileKey>,
}
impl ProfileProvenance {
    pub(super) fn admit(
        &mut self,
        admission: ApplyAdmission,
        draft: &Draft,
        binding: ProfileBinding,
        limit: usize,
    ) {
        let submitted = retain_settings(&draft.settings, limit);
        let (origin, matches_loaded) = self
            .selection_seed
            .as_ref()
            .map(|seed| seed.project(&submitted, limit))
            .unwrap_or((
                None,
                if submitted.is_ok() {
                    Ok(false)
                } else {
                    Err(dto::ProjectionOversized)
                },
            ));
        self.pending = Some(PendingProfile {
            admission,
            revision: draft.revision,
            submitted,
            binding,
            opening: None,
            origin,
            matches_loaded,
        });
    }
    pub(super) fn observe_opening(&mut self, key: AttemptKey) {
        if let Some(pending) = &mut self.pending
            && matches!(pending.admission, ApplyAdmission::Open { apply } if apply == key.apply)
            && key.purpose == AttemptPurpose::Candidate
        {
            pending.opening = Some(key);
        }
    }
    pub(super) fn verified(&mut self, event: &VerifiedApplied, limit: usize) {
        let Some(pending) = &self.pending else { return };
        let (applied, comparable) = match (pending.admission, event) {
            (ApplyAdmission::Open { apply }, VerifiedApplied::Open { key, applied })
                if apply == key.apply
                    && key.purpose == AttemptPurpose::Candidate
                    && pending.opening == Some(*key) =>
            {
                (
                    applied,
                    same_configuration(
                        pending.binding,
                        &pending.submitted,
                        applied.settings(),
                        limit,
                    ),
                )
            }
            (
                ApplyAdmission::Treatments {
                    key: admitted,
                    revision: admitted_revision,
                },
                VerifiedApplied::Treatments {
                    key,
                    revision,
                    applied,
                },
            ) if admitted == *key
                && key.pass == crate::domain::state::TreatmentPass::Candidate
                && admitted_revision == *revision
                && pending.revision == *revision =>
            {
                let same =
                    comparable(&pending.submitted, applied.settings(), limit).map(|submitted| {
                        applied.settings().same_request(submitted)
                            && applied.settings().video.resolved_mode().is_some()
                    });
                (applied, same)
            }
            _ => return,
        };
        if comparable == Ok(false) {
            return;
        }
        let verified = VerifiedProfile {
            // A successful comparison has preflighted BOTH operands already.
            settings: if comparable.is_ok() {
                Ok(applied.settings().clone())
            } else {
                Err(dto::ProjectionOversized)
            },
            binding: pending.binding,
            origin: pending.origin,
            matches_loaded: if comparable.is_ok() {
                pending.matches_loaded
            } else {
                Err(dto::ProjectionOversized)
            },
        };
        self.active = Some(verified.clone());
        self.last_valid = Some(verified);
        self.pending = None;
        self.last_active_key = None;
    }
    pub(super) fn cancel(&mut self) {
        self.pending = None;
    }
    pub(super) fn pending_apply(&self) -> Option<crate::domain::state::ApplyId> {
        self.pending.as_ref().map(|v| v.admission.id())
    }
    pub(super) fn synchronize(
        &mut self,
        active: Option<&AppliedSettings>,
        last_valid: Option<&AppliedSettings>,
        key: Option<ActiveProfileKey>,
        limit: usize,
    ) {
        if last_valid.is_none() {
            self.last_valid = None;
        }
        let Some(applied) = active else {
            self.active = None;
            self.last_active_key = None;
            return;
        };
        if key.is_some() && self.last_active_key == key {
            return;
        }
        self.last_active_key = key;
        // Only already verified metadata may be reattached. Unknown comparison
        // is not rejection, but cannot keep an old KNOWN profile claim.
        if let Some(current) = &self.active {
            match current.matches(applied.settings(), limit) {
                Ok(true) => return,
                Err(_) => {
                    self.active = Some(current.unknown());
                    return;
                }
                Ok(false) => {}
            }
        }
        self.active = match &self.last_valid {
            Some(previous) => match previous.matches(applied.settings(), limit) {
                Ok(true) => Some(previous.clone()),
                Ok(false) => None,
                Err(_) => Some(previous.unknown()),
            },
            None => None,
        };
    }
    fn applied(&self) -> Result<Option<dto::AppliedProfile>, dto::ProjectionOversized> {
        self.active
            .as_ref()
            .map(VerifiedProfile::applied)
            .unwrap_or(Ok(None))
    }
}

/// These inputs are compared by reference on ordinary polls. Clones happen only
/// when facts change; native strings and inventory are not rebuilt for ticks.
pub(super) struct ReportCache {
    pub(super) environment: dto::EnvironmentReport,
    ui_health: Option<crate::diagnostics::DiagnosticsHealth>,
    pub(super) provenance: ProfileProvenance,
    last_inputs: Option<Inputs>,
    last_session: dto::SessionReport,
    pub(super) revision: dto::ReportRevision,
    gate_phase: &'static str,
    generation: Option<u64>,
    environment_dirty: bool,
    publication_stopped: bool,
    #[cfg(test)]
    pub(super) publication_pause:
        Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}
impl Default for ReportCache {
    fn default() -> Self {
        Self {
            environment: dto::EnvironmentReport::default(),
            provenance: ProfileProvenance::default(),
            last_inputs: None,
            last_session: dto::SessionReport::default(),
            revision: dto::ReportRevision::default(),
            gate_phase: "Idle",
            generation: None,
            environment_dirty: false,
            publication_stopped: false,
            ui_health: None,
            #[cfg(test)]
            publication_pause: None,
        }
    }
}
#[derive(PartialEq)]
struct Inputs {
    state: Option<StateIdentity>,
    draft_revision: Option<DraftRevision>,
    selection_revision: u64,
    gate_phase: &'static str,
    output_revision: OutputRevision,
    generation: Option<u64>,
    audio: dto::AudioReport,
    media: dto::RuntimeMediaReport,
    timing: Option<crate::media::gate::ReceiptTimingFacts>,
    failures: crate::app::diagnostic::DiagnosticFailureContexts,
    profile: Result<Option<dto::AppliedProfile>, dto::ProjectionOversized>,
}
impl RuntimeCoordinator {
    pub(super) fn diagnostics_projection(&self) -> super::projection::Diagnostics {
        use crate::diagnostics::{FolderState, SinkStatus};
        let health = &self.report_cache.environment.log_location;
        super::projection::Diagnostics {
            status: if !health.logging_enabled {
                "Disabled"
            } else {
                match health.sink_status {
                    SinkStatus::Starting => "Starting",
                    SinkStatus::Ready if health.has_problem() => "Degraded",
                    SinkStatus::Ready => "Healthy",
                    SinkStatus::Unavailable => "Unavailable",
                    SinkStatus::Stopped => "Stopped",
                }
            },
            path: health
                .location
                .as_ref()
                .map(|location| location.path.clone())
                .unwrap_or_default(),
            message: health.message_fr(),
            has_problem: health.has_problem(),
            dropped_events: health.dropped_events.to_string(),
            oversized_records: health.oversized_records.to_string(),
            oversized_reports: health.oversized_reports.to_string(),
            folder_state: match health.folder.state {
                FolderState::Idle => "Idle",
                FolderState::Preparing => "Preparing",
                FolderState::OpenRequested => "OpenRequested",
                FolderState::Failed => "Failed",
            },
        }
    }
    /// No I/O or consumption of any event/receipt/one-shot native edge.
    pub(crate) fn diagnostics_report(&self) -> dto::ReportSnapshot {
        let cached = &self.report_cache.environment;
        let environment = dto::EnvironmentReport {
            furami_version: cached.furami_version,
            os: cached.os.clone(),
            desktop_session: cached.desktop_session.clone(),
            log_location: cached.log_location.clone(),
            build_inventory: cached.build_inventory.clone(),
            native: cached.native.clone(),
            runtime_media: self
                .engine
                .as_ref()
                .and_then(|v| v.runner().diagnostics_observation())
                .map(dto::RuntimeMediaReport::from)
                .unwrap_or_default(),
            runtime_treatment_capabilities: cached.runtime_treatment_capabilities.clone(),
        };
        dto::ReportSnapshot {
            revision: self.report_cache.revision,
            environment,
            session: self.assemble_session_report(),
        }
    }
    fn audio_projection(&self) -> dto::AudioProjection<'_> {
        let engine = self.engine.as_ref();
        let helpers = engine
            .and_then(|engine| engine.runner().diagnostics_observation())
            .map(|observation| observation.helpers.into())
            .unwrap_or_default();
        dto::AudioProjection::new(
            self.output.choice(),
            self.output.plan(),
            engine.and_then(|engine| engine.audio_availability()),
            helpers,
        )
    }
    fn assemble_session_report(&self) -> dto::SessionReport {
        let mut report = dto::SessionReport {
            phase: "Stopped".into(),
            gate_phase: self.report_cache.gate_phase,
            audio: self.audio_projection().into_report(),
            ..dto::SessionReport::default()
        };
        if let Some(partial) = &self.ui.partial
            && let (Some(physical), Some(size), Some(rate)) =
                (&partial.source, partial.size, partial.rate)
        {
            report.selected_capture = dto::Observation::Observed {
                value: dto::CaptureTuple::new(
                    physical.identity.vendor_id(),
                    physical.identity.product_id(),
                    CaptureModeRequest {
                        format_policy: partial.policy,
                        size,
                        rate,
                    },
                    None,
                ),
            };
        }
        let Some(engine) = &self.engine else {
            return report;
        };
        let model = engine.model();
        let identity = model.state_identity();
        report.identity = dto::SessionIdentity {
            operation: identity.operation().map(|v| v.get()),
            attempt: identity.attempt().map(|v| v.get()),
            generation: self.report_cache.generation,
        };
        report.phase = dto::phase_code(model.phase()).into();
        report.opening = model.opening().map(|(key, _)| key.into());
        if self.ui.partial.is_none() {
            report.selected_capture = dto::Observation::Observed {
                value: (&model.draft().settings.video).into(),
            };
        }
        if let Some(active) = model.active() {
            report.applied_capture = dto::Observation::Observed {
                value: (&active.applied().settings().video).into(),
            };
            // confirmed_treatments deliberately survives stop in the domain;
            // only the matching actual active owner authorizes this projection.
            if let Some(confirmed) = model
                .confirmed_treatments()
                .filter(|v| v.key().attempt == active.attempt())
            {
                report.treatments.active_renderer = dto::Observation::Observed {
                    value: confirmed.renderer().settings().into(),
                };
            }
            if let Some(facts) = engine.runner().receipt_timing_facts().filter(|v| {
                v.key.attempt == active.attempt() && v.generation.get() == active.attempt().get()
            }) {
                report.timing = dto::TimingReport::from_receipt(facts);
            }
            match self.report_cache.provenance.applied() {
                Ok(profile) => report.applied_profile = profile,
                Err(_) => report.projection_oversized = true,
            }
        }
        report.treatments.requested_renderer = Some((&model.draft().settings.renderer).into());
        report.treatments.pending = model.treating().map(|v| v.key().into());
        report.failures = engine.diagnostic_failures().into();
        report
    }
    pub(super) fn poll_diagnostic_environment(&mut self) -> bool {
        let mut dirty = std::mem::take(&mut self.report_cache.environment_dirty);
        if let Some((os, health)) = self.diagnostics.environment_if_changed(
            &self.report_cache.environment.os,
            &self.report_cache.environment.log_location,
        ) {
            self.report_cache.environment.os = os;
            self.report_cache.environment.log_location = health;
            dirty = true;
        }
        // Publication may already have folded its own loss accounting into the
        // report baseline. UI delivery has a separate baseline and must not
        // suppress that warning, nor generate another product log.
        let health = &self.report_cache.environment.log_location;
        if self.report_cache.ui_health.as_ref() != Some(health) {
            self.report_cache.ui_health = Some(health.clone());
            self.dirty = true;
        }
        dirty
    }
    pub(super) fn publish_diagnostics(
        &mut self,
        environment_changed: bool,
        phase: GatePhase,
        generation: u64,
    ) {
        let gate_phase = match phase {
            GatePhase::Idle => "Idle",
            GatePhase::WaitingSurface => "WaitingSurface",
            GatePhase::Opening => "Opening",
            GatePhase::Ready => "Ready",
            GatePhase::Stopping => "Stopping",
            GatePhase::Releasing => "Releasing",
            GatePhase::Failed => "Failed",
            GatePhase::QuitReady => "QuitReady",
        };
        self.report_cache.gate_phase = gate_phase;
        let generation = (generation != 0).then_some(generation);
        self.report_cache.generation = generation;
        if self.report_cache.publication_stopped {
            return;
        }
        let engine = self.engine.as_ref();
        let state = engine.map(|v| v.model().state_identity());
        let draft_revision = engine.map(|v| v.model().draft().revision);
        let audio = self.audio_projection();
        let media = engine.and_then(|v| v.runner().diagnostics_observation());
        let timing = engine.and_then(|v| v.runner().receipt_timing_facts());
        let empty_failures = crate::app::diagnostic::DiagnosticFailureContexts::default();
        let failures = engine
            .map(|v| v.diagnostic_failures())
            .unwrap_or(&empty_failures);
        let profile = self.report_cache.provenance.applied();
        let changed = self.report_cache.last_inputs.as_ref().is_none_or(|last| {
            last.state != state
                || last.draft_revision != draft_revision
                || last.selection_revision != self.ui.public_revision
                || last.gate_phase != gate_phase
                || last.generation != generation
                || last.output_revision != self.output.revision()
                || !audio.matches(&last.audio)
                || !last.media.matches_observation(media)
                || last.timing.as_ref() != timing
                || &last.failures != failures
                || last.profile != profile
        });
        if !environment_changed && !changed {
            return;
        }
        if changed {
            let inputs = Inputs {
                state,
                draft_revision,
                selection_revision: self.ui.public_revision,
                gate_phase,
                generation,
                output_revision: self.output.revision(),
                audio: audio.into_report(),
                media: media.map(dto::RuntimeMediaReport::from).unwrap_or_default(),
                timing: timing.copied(),
                failures: failures.clone(),
                profile,
            };
            self.report_cache.last_inputs = Some(inputs);
        }
        let mut report = self.diagnostics_report();
        let semantic_changed = environment_changed
            || self.report_cache.last_session != report.session
            || self.report_cache.environment.runtime_media != report.environment.runtime_media
            || self.report_cache.revision.0 == 0;
        if !semantic_changed {
            return;
        }
        self.report_cache.environment.runtime_media = report.environment.runtime_media.clone();
        self.report_cache.last_session = report.session.clone();
        if let Some(revision) = self.diagnostics.reserve_report_revision() {
            self.report_cache.revision = revision;
            report.revision = revision;
            #[cfg(test)]
            if let Some((reached, resume)) = self.report_cache.publication_pause.take() {
                reached.send(()).expect("publication race observer");
                resume
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("publication race release");
            }
            let effects = self.diagnostics.publish_report_with_effects(report);
            // This was the pre-publication sample. Advance ONLY this call's
            // exact mutations; never sample/adopt whole post-publication health.
            self.report_cache.environment_dirty |=
                effects.fold_into(&mut self.report_cache.environment.log_location);
        } else {
            // The shared allocator has already retained its typed terminal
            // CounterExhausted health. Never retry/relabel an old context.
            self.report_cache.publication_stopped = true;
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn native_environment(
        &mut self,
        qt_version: &str,
        platform: &str,
        x_vendor: &str,
        x_release: u32,
        x_protocol_major: u32,
        x_protocol_minor: u32,
        shape_major: u32,
        shape_minor: u32,
    ) -> UiUpdate {
        let observed = dto::NativeEnvironmentReport {
            qt_runtime_version: dto::Observation::Observed {
                value: dto::bounded_text(qt_version, 128),
            },
            qt_platform: dto::Observation::Observed {
                value: dto::bounded_text(platform, 128),
            },
            x_server: dto::Observation::Observed {
                value: dto::XServerReport {
                    vendor: dto::bounded_text(x_vendor, 256),
                    release: x_release,
                    protocol_major: x_protocol_major,
                    protocol_minor: x_protocol_minor,
                    shape_major,
                    shape_minor,
                },
            },
            xwayland_version: dto::Observation::Unavailable {
                reason: dto::UnavailableReason::NotExposed,
            },
        };
        if self.report_cache.environment.native != observed {
            self.report_cache.environment.native = observed;
            self.report_cache.environment_dirty = true;
        }
        self.poll()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capture::{DeviceIdentity, UsbTopology};

    fn settings() -> DraftSettings {
        let mode = crate::domain::capture::CaptureMode {
            captured_fourcc: crate::domain::capture::CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(1280, 720).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        let identity = crate::capture::linux::session_fixture(&["/dev/video-private"], mode)
            .devices()[0]
            .identity()
            .clone();
        DraftSettings {
            video: crate::domain::capture::VideoRequest::manual(
                crate::domain::capture::ModeRequest { identity, mode },
            ),
            audio: AudioSelection::default(),
            renderer: RendererSettings::default(),
        }
    }

    #[test]
    fn retained_private_settings_bound_every_owned_collection_string_and_path_before_comparison() {
        const LIMIT: usize = 4_096;
        let bounded = retain_settings(&settings(), LIMIT);
        assert!(bounded.is_ok());
        for field in 2..10 {
            let mut candidate = settings();
            match field {
                2..=4 => {
                    let original = &candidate.video.identity;
                    let controller = if field == 2 {
                        "private-controller".repeat(LIMIT)
                    } else {
                        original.topology().controller().to_owned()
                    };
                    let ports = if field == 4 {
                        vec![std::num::NonZeroU8::new(1).unwrap(); LIMIT + 1]
                    } else {
                        original.topology().ports().to_vec()
                    };
                    let serial = if field == 3 {
                        Some("private-serial".repeat(LIMIT))
                    } else {
                        original.serial().map(str::to_owned)
                    };
                    candidate.video.identity = DeviceIdentity::new(
                        original.vendor_id(),
                        original.product_id(),
                        UsbTopology::new(controller, ports).unwrap(),
                        serial,
                    )
                    .unwrap();
                }
                5 => {
                    candidate.video.selected_node =
                        Some(format!("/dev/{}", "private-path".repeat(LIMIT)).into())
                }
                6..=9 => {
                    let name = if field == 6 {
                        "private-audio-name".repeat(LIMIT)
                    } else {
                        "audio".into()
                    };
                    let properties = match field {
                        7 => (0..LIMIT)
                            .map(|index| (format!("key-{index}"), "v".into()))
                            .collect(),
                        8 => vec![("private-key".repeat(LIMIT), "v".into())],
                        9 => vec![("key".into(), "private-value".repeat(LIMIT))],
                        _ => Vec::new(),
                    };
                    candidate.audio = AudioSelection::Disabled {
                        retained: Some(AudioSourceIdentity::new(name, properties).unwrap()),
                    };
                }
                _ => unreachable!(),
            }
            assert!(
                retain_settings(&candidate, LIMIT).is_err(),
                "owned field {field} exceeded bounded retained storage"
            );
            assert!(
                comparable(&bounded, &candidate, LIMIT).is_err(),
                "new borrowed field {field} must be preflighted before equality"
            );
            assert!(
                same_configuration(ProfileBinding::Serial, &bounded, &candidate, LIMIT).is_err()
            );
            let seed = ProfileSeed::new(
                &candidate,
                ProfileBinding::Serial,
                dto::ProfileOrigin::Startup,
                LIMIT,
            );
            assert!(seed.loaded.is_err());
            let (origin, matches) = seed.project(&bounded, LIMIT);
            assert_eq!(
                origin,
                Some((ProfileBinding::Serial, dto::ProfileOrigin::Startup))
            );
            assert!(
                matches.is_err(),
                "oversized origin is unknown, not absent or a manufactured mismatch"
            );
        }
    }

    #[test]
    fn bounded_loaded_comparison_preserves_renderer_disabled_audio_and_request_semantics() {
        let mut original = settings();
        original.audio = AudioSelection::Disabled {
            retained: Some(
                AudioSourceIdentity::new("audio".into(), vec![("key".into(), "value".into())])
                    .unwrap(),
            ),
        };
        let seed = ProfileSeed::new(
            &original,
            ProfileBinding::Serial,
            dto::ProfileOrigin::Startup,
            131_072,
        );
        let mut submitted = original.clone();
        submitted.video.selected_node = Some("/dev/other-node".into());
        submitted.video.resolved_fourcc = None;
        assert_eq!(
            seed.project(&retain_settings(&submitted, 131_072), 131_072)
                .1,
            Ok(true),
            "same_request ignores locators and resolution"
        );
        let mut values = submitted.renderer.values();
        values.interpolation = true;
        submitted.renderer = RendererSettings::new(values).unwrap();
        assert_eq!(
            seed.project(&retain_settings(&submitted, 131_072), 131_072)
                .1,
            Ok(false)
        );
        submitted = original.clone();
        submitted.audio = AudioSelection::Disabled {
            retained: Some(
                AudioSourceIdentity::new("audio".into(), vec![("key".into(), "edited".into())])
                    .unwrap(),
            ),
        };
        assert_eq!(
            seed.project(&retain_settings(&submitted, 131_072), 131_072)
                .1,
            Ok(false),
            "disabled retained audio properties still matter"
        );
    }
}
