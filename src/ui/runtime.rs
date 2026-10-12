//! Qt-thread composition: product coordinator plus capture/media adapters.

use super::{catalog as capture_catalog, projection};
use crate::domain::{
    capture::{CaptureModeRequest, FrameRate, FrameSize, InputFormatPolicy},
    filters::{FilterChain, FilterEntry, FilterKind},
    render::{DitherMode, RendererSettings, ScalerChoice, ScalerKernel},
};

use crate::{
    app::{
        apply::{ApplyCoordinator, VerifiedApplied},
        control::{self, Command, ExpectedCleanup, ExpectedState},
        gate::GatePhase,
        output::OutputPolicy,
        ports::SubmitStatus,
        settings::{PersistenceSession, StartupSelection},
    },
    capture::CaptureValidator,
    domain::{
        capture::{
            AudioAvailability, AudioSelection, AudioSilence, AudioSourceIdentity, CandidateId,
            LossEvidence, ObservationEpoch, PlaybackGain, RecoveryCandidate, SelectionToken,
            WatchId, WatchStamp,
        },
        output::{
            LiveSinkTarget, OutputRevision, OutputSilence, PersistentOutputChoice, SinkCatalog,
            SinkIdentity,
        },
        state::{
            AttemptId, CleanupStatus, CommandRejection, DraftRevision, DraftSettings,
            InitialPlayback, PlaybackState, ProductModel, ProductPhase, StateIdentity,
        },
    },
    media::{controller::SurfaceToken, gate::GateRunner, output_catalog::OutputCatalogWatch},
    settings::ProfileBinding,
};

type Engine = ApplyCoordinator<CaptureValidator, GateRunner>;

pub(crate) struct UiUpdate {
    pub ui_snapshot: String,
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
    pub context_diagnostic: String,
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
    pub settings_status: String,
    pub settings_path: String,
    pub settings_refused: bool,
    pub saved_selection: String,
    pub startup_reason: String,
    pub close_dialog: String,
    pub reset_token: u64,
    pub fullscreen: bool,
    pub stats_hidden: bool,
    pub closing: bool,
    /// Legacy audio output selector projection: JSON plain-text rows (the
    /// reserved `auto` row first), the observed catalog revision and selected
    /// action key, the
    /// confirmed routed sink name, the typed silence reason text and whether
    /// the runtime needs an explicit reselection. Plain text only; empty
    /// fields mean unchanged since the last published update.
    pub output_rows: String,
    pub output_catalog_revision: u64,
    pub output_selected: String,
    /// Opaque action key of the current choice: "auto" for the reserved row,
    /// the unique compatible catalog row key for a manual choice, empty when
    /// the manual choice matches no observed sink (unavailable). Actions
    /// echo keys, never names.
    pub output_selected_key: String,
    pub output_effective: String,
    pub output_status: String,
    pub output_needs_action: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct PartialDraft {
    source: Option<capture_catalog::Physical>,
    size: Option<FrameSize>,
    rate: Option<FrameRate>,
    policy: InputFormatPolicy,
    audio: AudioSelection,
    filters: FilterChain,
    renderer: RendererSettings,
}
impl Default for PartialDraft {
    fn default() -> Self {
        Self {
            source: None,
            size: None,
            rate: None,
            policy: InputFormatPolicy::Auto,
            audio: AudioSelection::default(),
            filters: FilterChain::default(),
            renderer: RendererSettings::default(),
        }
    }
}
impl PartialDraft {
    fn from_settings(settings: &DraftSettings) -> Self {
        Self {
            size: Some(settings.video.mode.size),
            rate: Some(settings.video.mode.rate),
            policy: settings.video.mode.format_policy,
            audio: settings.audio.clone(),
            filters: settings.filters.clone(),
            renderer: settings.renderer.clone(),
            ..Self::default()
        }
    }
}
#[derive(Clone, Copy)]
enum SelectionOrigin {
    User { new_source: bool },
    Startup { original: CaptureModeRequest },
}
struct PendingSelection {
    serial: u64,
    catalog_revision: u64,
    selection_revision: u64,
    domain_revision: Option<DraftRevision>,
    partial: PartialDraft,
    binding: ProfileBinding,
    intent: capture_catalog::ModeIntent,
    origin: SelectionOrigin,
}
struct ProductUi {
    catalog: capture_catalog::Model,
    bootstrap_runner: Option<GateRunner>,
    partial: Option<PartialDraft>,
    selected: Option<capture_catalog::Physical>,
    selected_binding: Option<(crate::domain::capture::DeviceIdentity, ProfileBinding)>,
    source_explicit: bool,
    submitted: Option<(
        crate::domain::state::ApplyId,
        Option<capture_catalog::Physical>,
    )>,
    resolving: Option<PendingSelection>,
    audio_policy: crate::app::settings::StartupAudioPolicy,
    audio_reason: String,
    cli_audio: Option<AudioSelection>,
    profile_audio: bool,
    startup_adaptation: bool,
    startup_pin: Option<(std::path::PathBuf, std::path::PathBuf)>,
    public_revision: u64,
    domain_revision: Option<DraftRevision>,
    state_sequence: u64,
    state_identity: Option<StateIdentity>,
    state_initialized: bool,
    filters: projection::Filters,
    renderer: projection::Renderer,
    snapshot: Option<projection::Snapshot>,
    error: Option<projection::IntentError>,
    exhausted: bool,
}
impl Default for ProductUi {
    fn default() -> Self {
        Self {
            catalog: capture_catalog::Model::default(),
            bootstrap_runner: None,
            partial: None,
            selected: None,
            selected_binding: None,
            source_explicit: false,
            resolving: None,
            audio_policy: crate::app::settings::StartupAudioPolicy::Explicit,
            audio_reason: String::new(),
            cli_audio: None,
            profile_audio: false,
            startup_adaptation: false,
            startup_pin: None,
            submitted: None,
            public_revision: 1,
            domain_revision: None,
            state_sequence: 1,
            state_identity: None,
            state_initialized: false,
            filters: projection::Filters::default(),
            renderer: projection::Renderer::default(),
            snapshot: None,
            error: None,
            exhausted: false,
        }
    }
}

pub(crate) struct RuntimeCoordinator {
    engine: Option<Engine>,
    ui: ProductUi,
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
    persistence: PersistenceSession,
    auto_open: bool,
    startup_apply: Option<crate::domain::state::ApplyId>,
    /// Application-owned desired-output policy fed by the catalog watch.
    output: OutputPolicy,
    /// Application-lifetime catalog supervisor; Pulse waits stay on its worker.
    catalog: Option<OutputCatalogWatch>,
    /// The initial watch spawn failed and cannot recover on this host.
    catalog_start_failed: bool,
    /// Last live target registered with the watch; an unchanged target is
    /// never re-registered outside an explicit selection.
    registered_target: Option<LiveSinkTarget>,
    /// Last desired revision fed to the engine, so a new revision is never
    /// dropped between polls.
    fed_plan_revision: Option<OutputRevision>,
    /// Last successfully observed catalog, kept for explicit selection
    /// resolution and UI rows.
    last_catalog: Option<SinkCatalog>,
    /// Row descriptors behind the serialized projection rows: opaque
    /// revision-scoped keys mapped to their exact observed identities.
    last_rows: Vec<OutputRow>,
    last_output: Option<OutputProjection>,
    /// Shutdown requested for the catalog worker; a stopped watch is never
    /// restarted and only its completion closes the quit barrier.
    catalog_stopping: bool,
    /// Terminal join failure of the catalog worker, reported once.
    catalog_join_error: Option<String>,
}

/// Reserved key of the reserved Auto row. Never derivable from a sink name:
/// catalogued sinks always get opaque revision-scoped keys.
const AUTO_ROW_KEY: &str = "auto";

/// Exact product-limitation notice surfaced through the existing bounded
/// `diagnostic` presentation bridge whenever interpolation is selected/applied.
/// Interpolation is stored and applied but inactive under the current
/// low-latency timing; the timing options are deliberately unchanged.
const INTERPOLATION_INACTIVE_TEXT: &str =
    "Interpolation is stored but inactive with the current low-latency timing.";

/// One selector row. `identity` stays runtime-side (never serialized): the
/// opaque `key` is the only thing the UI echoes back for actions. `name` is
/// serialized so the UI can match a saved manual choice against observed
/// sinks for display and reconnection, never as an action key.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct OutputRow {
    key: String,
    label: String,
    name: String,
    eligible: bool,
    #[serde(skip)]
    identity: Option<SinkIdentity>,
}

/// Presentation projection of the output selector, compared whole between
/// polls so an unchanged selector never republishes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OutputProjection {
    rows: String,
    catalog_revision: u64,
    selected: String,
    selected_key: String,
    effective: String,
    status: String,
    needs_action: bool,
}
/// Plain status text for a typed output silence. Shared by the selector
/// projection and the audio-availability diagnostic so both surfaces word
/// the same reason identically.
fn output_silence_text(reason: &OutputSilence) -> String {
    match reason {
        OutputSilence::CatalogUnavailable(error) => {
            format!("Output catalog unavailable: {error}")
        }
        OutputSilence::NoAvailableOutput => "No available output".to_owned(),
        OutputSilence::ManualUnavailable => "Selected output is unavailable".to_owned(),
        OutputSilence::ManualRequiresAction => {
            "Select this output again to reconnect it".to_owned()
        }
        OutputSilence::RoutingConflict(error) => format!("Routing conflict: {error}"),
    }
}

impl RuntimeCoordinator {
    fn ui_reject(
        &mut self,
        code: &'static str,
        action: &'static str,
        field: &'static str,
        message: impl Into<String>,
    ) {
        self.ui.error = Some(projection::IntentError {
            code,
            action,
            field,
            message: message.into(),
        });
        self.dirty = true;
    }
    fn ui_guard(
        &self,
        revision: &str,
        action: &'static str,
    ) -> Result<(), projection::IntentError> {
        let error = |code, message: &str| projection::IntentError {
            code,
            action,
            field: "selectionRevision",
            message: message.into(),
        };
        if !self.persistence.mutations_open() {
            return Err(error("closing", "L'application est en cours de fermeture."));
        }
        if self.ui.exhausted || self.ui.public_revision == u64::MAX {
            return Err(error(
                "unavailable",
                "La séquence de révisions est épuisée.",
            ));
        }
        if capture_catalog::decimal(revision) != Some(self.ui.public_revision)
            || self
                .engine
                .as_ref()
                .map(|engine| engine.model().draft().revision)
                != self.ui.domain_revision
        {
            return Err(error(
                "stale_selection",
                "La sélection a changé ; utilisez sa révision actuelle.",
            ));
        }
        Ok(())
    }
    fn current_partial(&self) -> PartialDraft {
        if let Some(partial) = &self.ui.partial {
            return partial.clone();
        }
        match &self.engine {
            Some(engine) => {
                let settings = &engine.model().draft().settings;
                PartialDraft {
                    source: self.ui.selected.clone(),
                    ..PartialDraft::from_settings(settings)
                }
            }
            None => PartialDraft::default(),
        }
    }
    fn settings_from_partial(
        &self,
        partial: &PartialDraft,
    ) -> Result<Option<DraftSettings>, String> {
        let (Some(size), Some(rate)) = (partial.size, partial.rate) else {
            return Ok(None);
        };
        let mode = CaptureModeRequest {
            format_policy: partial.policy,
            size,
            rate,
        };
        let existing = self
            .engine
            .as_ref()
            .map(|engine| &engine.model().draft().settings);
        let source_changed = partial.source != self.ui.selected;
        let video = match existing {
            Some(existing) if !source_changed && self.ui.partial.is_none() => {
                let mut request = existing.video.clone();
                if request.mode != mode {
                    request.mode = mode;
                    request.resolved_fourcc = match mode.format_policy {
                        InputFormatPolicy::Auto => None,
                        InputFormatPolicy::Manual { fourcc } => Some(fourcc),
                    };
                }
                if let Some(physical) = &partial.source {
                    request.selected_node = self
                        .ui
                        .catalog
                        .source(physical)
                        .map(|source| source.node.devnode().to_owned());
                }
                request
            }
            _ => {
                let physical = partial
                    .source
                    .as_ref()
                    .ok_or("Sélectionnez un nœud de capture explicite.")?;
                let source = self
                    .ui
                    .catalog
                    .source(physical)
                    .ok_or("Le nœud physique choisi n'est plus disponible.")?;
                let snapshot = self
                    .ui
                    .catalog
                    .snapshot
                    .as_ref()
                    .ok_or("Le catalogue de capture est indisponible.")?;
                let mut request = crate::capture::input::CaptureSelection::request_from_snapshot(
                    snapshot,
                    source.node.devnode(),
                    mode,
                )
                .map_err(|error| error.to_string())?;
                if let InputFormatPolicy::Manual { fourcc } = mode.format_policy {
                    request.resolved_fourcc = Some(fourcc);
                }
                request
            }
        };
        Ok(Some(DraftSettings {
            video,
            audio: partial.audio.clone(),
            filters: partial.filters.clone(),
            renderer: partial.renderer.clone(),
        }))
    }
    fn advance_edit(&mut self) {
        match self.ui.public_revision.checked_add(1) {
            Some(revision) => self.ui.public_revision = revision,
            None => self.ui.exhausted = true,
        }
        self.ui.domain_revision = self
            .engine
            .as_ref()
            .map(|engine| engine.model().draft().revision);
        self.ui.error = None;
        self.dirty = true;
    }
    fn check_selection_ready(&self) -> Result<(), (&'static str, String)> {
        if !self.persistence.mutations_open() {
            return Err(("closing", "L'application est en cours de fermeture.".into()));
        }
        if self.ui.exhausted || self.ui.public_revision == u64::MAX {
            return Err((
                "unavailable",
                "La séquence de révisions est épuisée.".into(),
            ));
        }
        if self.ui.resolving.is_some() {
            return Err((
                "busy",
                "Le mode de la source choisie est en cours de résolution.".into(),
            ));
        }
        match &self.engine {
            Some(engine) => engine
                .check_apply_ready(
                    engine.model().state_identity(),
                    engine.model().draft().revision,
                )
                .map_err(|error| ("busy", error.to_string())),
            None if self.ui.bootstrap_runner.is_some() => Ok(()),
            None => Err((
                "unavailable",
                "Le propriétaire média n'est pas disponible.".into(),
            )),
        }
    }
    fn cancel_startup_selection(&mut self) {
        self.ui.startup_adaptation = false;
        if self
            .ui
            .resolving
            .as_ref()
            .is_some_and(|pending| matches!(pending.origin, SelectionOrigin::Startup { .. }))
        {
            self.ui.resolving = None;
            self.ui.catalog.cancel_mode();
            self.dirty = true;
        }
    }
    fn edit_ui(
        &mut self,
        revision: &str,
        action: &'static str,
        field: &'static str,
        change: impl FnOnce(
            &mut PartialDraft,
            &capture_catalog::Model,
        ) -> Result<(), (&'static str, String)>,
    ) -> UiUpdate {
        if let Err(error) = self.ui_guard(revision, action) {
            self.ui.error = Some(error);
            self.dirty = true;
            return self.poll();
        }
        self.cancel_startup_selection();
        if let Err((code, error)) = self.check_selection_ready() {
            self.ui_reject(code, action, field, error);
            return self.poll();
        }
        let mut partial = self.current_partial();
        if let Err((code, message)) = change(&mut partial, &self.ui.catalog) {
            self.ui_reject(code, action, field, message);
            return self.poll();
        }
        self.commit_selection(partial, action, field, None, None);
        self.poll()
    }
    pub(crate) fn refresh_catalogs(&mut self) -> UiUpdate {
        if self.persistence.mutations_open() {
            self.ui.catalog.refresh();
            self.dirty = true;
        }
        self.poll()
    }
    pub(crate) fn select_capture(&mut self, revision: &str, catalog: &str, row: &str) -> UiUpdate {
        const ACTION: &str = "selectCapture";
        if let Err(error) = self.ui_guard(revision, ACTION) {
            self.ui.error = Some(error);
            self.dirty = true;
            return self.poll();
        }
        self.cancel_startup_selection();
        if let Err((code, error)) = self.check_selection_ready() {
            self.ui_reject(code, ACTION, "source", error);
            return self.poll();
        }
        let physical = match self.ui.catalog.resolve_source(catalog, row) {
            Ok(source) if source.supports_input() => source.physical(),
            Ok(_) => {
                self.ui_reject(
                    "unavailable",
                    ACTION,
                    "source",
                    "Ce nœud n'a aucun format mono-planaire compatible.",
                );
                return self.poll();
            }
            Err(error) => {
                self.ui_reject("stale_catalog", ACTION, "source", error);
                return self.poll();
            }
        };
        self.observe_profile_snapshot();
        let serial_unique = self.serial_unique(&physical.identity);
        let profile = self
            .persistence
            .source_profile(&physical.identity, serial_unique);
        let new_source = profile.is_none();
        let mut partial = profile.map(PartialDraft::from_settings).unwrap_or_default();
        if let Some(audio) = &self.ui.cli_audio {
            partial.audio = audio.clone();
        }
        partial.source = Some(physical.clone());
        let binding = self
            .persistence
            .profile_binding(&physical.identity, serial_unique);
        self.begin_source_resolution(partial, binding, SelectionOrigin::User { new_source });
        self.poll()
    }
    fn observe_profile_snapshot(&mut self) {
        if self.ui.catalog.fresh
            && let Some(snapshot) = &self.ui.catalog.snapshot
        {
            self.persistence.observe_profile_identities(
                snapshot.devices().iter().map(|device| device.identity()),
            );
        }
    }
    fn serial_unique(&self, identity: &crate::domain::capture::DeviceIdentity) -> bool {
        self.ui.catalog.fresh
            && identity.serial().is_some()
            && self.ui.catalog.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot
                    .devices()
                    .iter()
                    .filter(|device| {
                        let observed = device.identity();
                        observed.vendor_id() == identity.vendor_id()
                            && observed.product_id() == identity.product_id()
                            && observed.serial() == identity.serial()
                    })
                    .count()
                    == 1
            })
    }
    fn begin_source_resolution(
        &mut self,
        mut partial: PartialDraft,
        binding: ProfileBinding,
        origin: SelectionOrigin,
    ) {
        let physical = partial.source.as_ref().expect("resolved physical source");
        let prior = partial
            .size
            .zip(partial.rate)
            .map(|(size, rate)| CaptureModeRequest {
                format_policy: partial.policy,
                size,
                rate,
            });
        let intent = match origin {
            SelectionOrigin::User { new_source: true } => capture_catalog::ModeIntent::Strict,
            _ => capture_catalog::ModeIntent::RestoreProfile,
        };
        match self
            .ui
            .catalog
            .resolve_source_mode(physical, partial.policy, prior, intent)
        {
            Ok(capture_catalog::ModeResolution::Ready(mode)) => {
                partial.size = Some(mode.size);
                partial.rate = Some(mode.rate);
                partial.policy = mode.format_policy;
                self.commit_source_selection(partial, binding, origin);
            }
            Ok(capture_catalog::ModeResolution::Pending(serial)) => {
                self.ui.resolving = Some(PendingSelection {
                    serial,
                    catalog_revision: self.ui.catalog.revision,
                    selection_revision: self.ui.public_revision,
                    partial,
                    binding,
                    intent,
                    origin,
                    domain_revision: self
                        .engine
                        .as_ref()
                        .map(|engine| engine.model().draft().revision),
                });
                self.ui.error = None;
                self.dirty = true;
            }
            Err(error) => {
                if matches!(origin, SelectionOrigin::Startup { .. }) {
                    self.persistence.startup_failed(error.clone());
                }
                self.ui_reject("capture_error", "selectCapture", "source", error);
            }
        }
    }
    fn commit_source_selection(
        &mut self,
        mut partial: PartialDraft,
        binding: ProfileBinding,
        origin: SelectionOrigin,
    ) {
        let audio_reason = if matches!(origin, SelectionOrigin::User { new_source: true })
            && self.ui.cli_audio.is_none()
        {
            let (audio, reason) = self
                .ui
                .catalog
                .associated_audio(partial.source.as_ref().expect("resolved physical source"));
            partial.audio = audio;
            Some(reason)
        } else {
            None
        };
        self.commit_selection(
            partial,
            "selectCapture",
            "source",
            audio_reason,
            Some((binding, origin)),
        );
    }
    fn prepare_startup_adaptation(&mut self) {
        if !self.ui.startup_adaptation || !self.ui.catalog.fresh {
            return;
        }
        self.ui.startup_adaptation = false;
        if !self.persistence.mutations_open() || self.ui.resolving.is_some() {
            return;
        }
        self.observe_profile_snapshot();
        let result = (|| {
            self.check_selection_ready().map_err(|(_, error)| error)?;
            let settings = &self
                .engine
                .as_ref()
                .ok_or("Saved selection is unavailable.")?
                .model()
                .draft()
                .settings;
            let snapshot = self
                .ui
                .catalog
                .snapshot
                .as_ref()
                .ok_or("Capture catalog unavailable.")?;
            let index = crate::domain::capture::resolve_identity(
                &settings.video.identity,
                snapshot.devices().iter().map(|device| device.identity()),
            )
            .map_err(|error| error.to_string())?;
            let observed = snapshot.devices()[index].identity();
            let binding = self
                .persistence
                .saved_binding()
                .ok_or("Last source profile is unavailable.")?;
            if binding == ProfileBinding::Topology && observed != &settings.video.identity {
                return Err("The exact topology-bound last source is absent.".into());
            }
            let mut sources =
                self.ui.catalog.sources.iter().filter(|source| {
                    source.device.identity() == observed && source.supports_input()
                });
            let source = sources
                .next()
                .ok_or("No eligible video node for the last source.")?;
            if sources.next().is_some() {
                return Err(
                    "Several eligible video nodes require an explicit source selection.".into(),
                );
            }
            let mut partial = PartialDraft::from_settings(settings);
            partial.source = Some(source.physical());
            Ok((partial, binding, settings.video.mode))
        })();
        match result {
            Ok((partial, binding, original)) => {
                self.begin_source_resolution(
                    partial,
                    binding,
                    SelectionOrigin::Startup { original },
                );
            }
            Err(reason) => {
                self.persistence.startup_failed(reason);
                self.dirty = true;
            }
        }
    }
    fn finish_source_selection(&mut self) {
        let reply = self.ui.catalog.take_mode_reply();
        let Some(pending) = self.ui.resolving.as_ref() else {
            return;
        };
        let physical = pending.partial.source.as_ref().expect("pending source");
        if !self.persistence.mutations_open()
            || !self.ui.catalog.fresh
            || self.ui.catalog.revision != pending.catalog_revision
            || self.ui.catalog.source(physical).is_none()
        {
            self.ui.resolving = None;
            self.ui_reject(
                "stale_catalog",
                "selectCapture",
                "source",
                "Le catalogue a changé pendant la résolution du mode.",
            );
            self.ui.catalog.cancel_mode();
            return;
        }
        if pending.domain_revision
            != self
                .engine
                .as_ref()
                .map(|engine| engine.model().draft().revision)
        {
            self.ui.resolving = None;
            self.ui.catalog.cancel_mode();
            self.ui_reject(
                "stale_selection",
                "selectCapture",
                "source",
                "La sélection a changé pendant la résolution du mode.",
            );
            return;
        }
        let Some(reply) = reply else {
            return;
        };
        if reply.serial != pending.serial
            || reply.revision != pending.catalog_revision
            || &reply.physical != physical
        {
            return;
        }
        if let Err(error) = self.ui_guard(&pending.selection_revision.to_string(), "selectCapture")
        {
            self.ui.resolving = None;
            self.ui.catalog.cancel_mode();
            self.ui.error = Some(error);
            self.dirty = true;
            return;
        }
        let PendingSelection {
            mut partial,
            binding,
            intent,
            origin,
            ..
        } = self
            .ui
            .resolving
            .take()
            .expect("matching accepted resolution");
        debug_assert_eq!(
            intent,
            match origin {
                SelectionOrigin::User { new_source: true } => capture_catalog::ModeIntent::Strict,
                _ => capture_catalog::ModeIntent::RestoreProfile,
            }
        );
        if let Err((code, error)) = self.check_selection_ready() {
            self.ui_reject(code, "selectCapture", "source", error);
            return;
        }
        match reply.result {
            Ok(mode) => {
                partial.size = Some(mode.size);
                partial.rate = Some(mode.rate);
                partial.policy = mode.format_policy;
                let newer_error = self.ui.error.take();
                self.commit_source_selection(partial, binding, origin);
                if self.ui.error.is_none() {
                    self.ui.error = newer_error;
                }
            }
            Err(error) => {
                if matches!(origin, SelectionOrigin::Startup { .. }) {
                    self.persistence.startup_failed(error.clone());
                }
                self.ui_reject("capture_error", "selectCapture", "source", error);
            }
        }
    }
    pub(crate) fn select_size(&mut self, revision: &str, catalog: &str, row: &str) -> UiUpdate {
        self.edit_ui(revision, "selectSize", "size", |draft, model| {
            draft.size = Some(
                model
                    .resolve_size(catalog, row)
                    .map_err(|error| ("stale_catalog", error))?,
            );
            Ok(())
        })
    }
    pub(crate) fn set_size(
        &mut self,
        revision: &str,
        catalog: &str,
        width: &str,
        height: &str,
    ) -> UiUpdate {
        self.edit_ui(revision, "setSize", "size", |draft, model| {
            model
                .check(catalog, model.fresh)
                .map_err(|error| ("stale_catalog", error))?;
            let dimension = |text: &str| -> Result<u32, (&'static str, String)> {
                if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err((
                        "invalid_value",
                        "La dimension doit être un entier positif ASCII.".into(),
                    ));
                }
                text.parse()
                    .map_err(|_| ("invalid_value", "La dimension dépasse u32.".into()))
            };
            draft.size = Some(
                FrameSize::new(dimension(width)?, dimension(height)?)
                    .map_err(|error| ("invalid_value", error.to_string()))?,
            );
            Ok(())
        })
    }
    pub(crate) fn select_rate(&mut self, revision: &str, catalog: &str, row: &str) -> UiUpdate {
        self.edit_ui(revision, "selectRate", "rate", |draft, model| {
            draft.rate = Some(
                model
                    .resolve_rate(catalog, row)
                    .map_err(|error| ("stale_catalog", error))?,
            );
            Ok(())
        })
    }
    pub(crate) fn set_rate(&mut self, revision: &str, catalog: &str, exact: &str) -> UiUpdate {
        self.edit_ui(revision, "setRate", "rate", |draft, model| {
            model
                .check(catalog, model.fresh)
                .map_err(|error| ("stale_catalog", error))?;
            draft.rate = Some(
                FrameRate::parse_exact(exact)
                    .map_err(|error| ("invalid_value", error.to_string()))?,
            );
            Ok(())
        })
    }
    pub(crate) fn select_format(&mut self, revision: &str, catalog: &str, row: &str) -> UiUpdate {
        self.edit_ui(revision, "selectFormat", "format", |draft, model| {
            let policy = model
                .resolve_format(catalog, row)
                .map_err(|error| ("stale_catalog", error))?;
            if !model
                .projection
                .format_rows
                .iter()
                .any(|format| format.key == row && format.eligible)
            {
                return Err((
                    "unavailable",
                    "Ce format n'est pas disponible pour le nœud et l'entrée qualifiée.".into(),
                ));
            }
            draft.policy = policy;
            Ok(())
        })
    }
    pub(crate) fn set_audio_enabled(&mut self, revision: &str, enabled: bool) -> UiUpdate {
        self.edit_ui(revision, "setAudioEnabled", "audio", |draft, _| {
            draft.audio = if enabled {
                AudioSelection::Enabled {
                    source: draft.audio.source().cloned().ok_or((
                        "audio_error",
                        "Sélectionnez une entrée audio explicite avant de l'activer.".into(),
                    ))?,
                }
            } else {
                AudioSelection::Disabled {
                    retained: draft.audio.source().cloned(),
                }
            };
            Ok(())
        })
    }
    pub(crate) fn select_audio_input(
        &mut self,
        revision: &str,
        catalog: &str,
        row: &str,
    ) -> UiUpdate {
        self.edit_ui(
            revision,
            "selectAudioInput",
            "audioInput",
            |draft, model| {
                let source = model
                    .resolve_input(catalog, row)
                    .map_err(|error| ("stale_catalog", error))?
                    .identity
                    .clone();
                draft.audio = if draft.audio.enabled() {
                    AudioSelection::Enabled { source }
                } else {
                    AudioSelection::Disabled {
                        retained: Some(source),
                    }
                };
                Ok(())
            },
        )
    }
    pub(crate) fn set_filter_enabled(
        &mut self,
        revision: &str,
        entry: &str,
        enabled: bool,
    ) -> UiUpdate {
        self.edit_ui(revision, "setFilterEnabled", "filters", |draft, _| {
            let prefix = format!("filter:{revision}:");
            let index = entry
                .strip_prefix(&prefix)
                .and_then(capture_catalog::decimal)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or((
                    "stale_selection",
                    "L'entrée de filtre ne correspond plus à cette sélection.".into(),
                ))?;
            draft
                .filters
                .set_enabled(index, enabled)
                .map_err(|error| ("filter_error", error.to_string()))?;
            Ok(())
        })
    }
    pub(crate) fn remove_filter(&mut self, revision: &str, entry: &str) -> UiUpdate {
        self.edit_ui(revision, "removeFilter", "filters", |draft, _| {
            let prefix = format!("filter:{revision}:");
            let index = entry
                .strip_prefix(&prefix)
                .and_then(capture_catalog::decimal)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or((
                    "stale_selection",
                    "L'entrée de filtre ne correspond plus à cette sélection.".into(),
                ))?;
            draft
                .filters
                .remove(index)
                .map_err(|error| ("filter_error", error.to_string()))?;
            Ok(())
        })
    }
    pub(crate) fn add_filter(&mut self, revision: &str, kind: &str) -> UiUpdate {
        let available = self
            .ui
            .filters
            .kinds
            .iter()
            .any(|row| row.kind == kind && row.available);
        self.edit_ui(revision, "addFilter", "filters", |draft, _| {
            let kind = FilterKind::ALL
                .into_iter()
                .find(|value| value.as_str() == kind)
                .ok_or(("invalid_value", "Type de filtre inconnu.".into()))?;
            if !available {
                return Err((
                    "unavailable",
                    "Ce filtre n'est pas disponible dans le registre qualifié.".into(),
                ));
            }
            if draft
                .filters
                .entries()
                .iter()
                .any(|entry| entry.kind() == kind)
            {
                return Err((
                    "filter_error",
                    "Ce type de filtre est déjà présent ; ses entrées sont conservées.".into(),
                ));
            }
            let mut counter = 1u64;
            let label = loop {
                let label = format!("{}-{counter}", kind.as_str());
                if !draft
                    .filters
                    .entries()
                    .iter()
                    .any(|entry| entry.label() == label)
                {
                    break label;
                }
                counter = counter.checked_add(1).ok_or((
                    "filter_error",
                    "La séquence des libellés est épuisée.".into(),
                ))?;
            };
            draft
                .filters
                .insert(
                    draft.filters.entries().len(),
                    FilterEntry::new(label, projection::default_filter(kind), true),
                )
                .map_err(|error| ("filter_error", error.to_string()))?;
            Ok(())
        })
    }
    fn edit_renderer(
        &mut self,
        revision: &str,
        action: &'static str,
        field: &'static str,
        change: impl FnOnce(&mut crate::domain::render::RendererValues) -> Result<(), String>,
    ) -> UiUpdate {
        let available = self.ui.renderer.available;
        self.edit_ui(revision, action, field, |draft, _| {
            if !available {
                return Err((
                    "unavailable",
                    "Le catalogue de rendu qualifié est indisponible.".into(),
                ));
            }
            let mut values = draft.renderer.values();
            change(&mut values).map_err(|error| ("invalid_value", error))?;
            draft.renderer =
                projection::mutate_renderer(values).map_err(|error| ("renderer_error", error))?;
            Ok(())
        })
    }
    fn scaler_allowed(&self, field: &str, token: &str) -> bool {
        self.ui
            .renderer
            .fields
            .iter()
            .find(|row| row.key == field)
            .is_some_and(|row| {
                row.choices
                    .iter()
                    .any(|choice| choice.key == token && choice.eligible)
            })
    }
    pub(crate) fn set_scale(&mut self, revision: &str, kernel: &str) -> UiUpdate {
        let allowed = self.scaler_allowed("scale", kernel);
        self.edit_renderer(revision, "setScale", "scale", |values| {
            let kernel = ScalerKernel::from_token(kernel)
                .ok_or("Noyau inconnu (scale n'accepte pas auto).")?;
            if !allowed {
                return Err(
                    "Noyau annoncé mais inefficace ou indisponible sur le renderer qualifié."
                        .into(),
                );
            }
            values.scale = kernel;
            Ok(())
        })
    }
    pub(crate) fn set_cscale(&mut self, revision: &str, kernel: &str) -> UiUpdate {
        self.set_scaler_choice(revision, kernel, true)
    }
    pub(crate) fn set_dscale(&mut self, revision: &str, kernel: &str) -> UiUpdate {
        self.set_scaler_choice(revision, kernel, false)
    }
    fn set_scaler_choice(&mut self, revision: &str, kernel: &str, chroma: bool) -> UiUpdate {
        let allowed = self.scaler_allowed(if chroma { "cscale" } else { "dscale" }, kernel);
        self.edit_renderer(
            revision,
            if chroma { "setCscale" } else { "setDscale" },
            if chroma { "cscale" } else { "dscale" },
            |values| {
                let choice = if kernel == "auto" {
                    ScalerChoice::Inherit
                } else {
                    ScalerChoice::Kernel(ScalerKernel::from_token(kernel).ok_or("Noyau inconnu.")?)
                };
                if !allowed {
                    return Err(
                        "Noyau annoncé mais inefficace ou indisponible sur le renderer qualifié."
                            .into(),
                    );
                }
                if chroma {
                    values.cscale = choice;
                } else {
                    values.dscale = choice;
                }
                Ok(())
            },
        )
    }
    pub(crate) fn set_scale_antiring(&mut self, revision: &str, value: f64) -> UiUpdate {
        self.edit_renderer(revision, "setScaleAntiring", "scale_antiring", |values| {
            values.scale_antiring = value;
            Ok(())
        })
    }
    pub(crate) fn set_dither(&mut self, revision: &str, mode: &str) -> UiUpdate {
        self.edit_renderer(revision, "setDither", "dither", |values| {
            values.dither = *DitherMode::ALL
                .iter()
                .find(|value| value.as_str() == mode)
                .ok_or("Mode de dithering inconnu.")?;
            Ok(())
        })
    }
    pub(crate) fn set_deband_enabled(&mut self, revision: &str, enabled: bool) -> UiUpdate {
        self.edit_renderer(revision, "setDebandEnabled", "deband", |values| {
            values.deband.enabled = enabled;
            Ok(())
        })
    }
    pub(crate) fn set_deband_iterations(&mut self, revision: &str, value: i32) -> UiUpdate {
        self.edit_renderer(
            revision,
            "setDebandIterations",
            "deband_iterations",
            |values| {
                values.deband.iterations = u8::try_from(value)
                    .map_err(|_| "Le nombre d'itérations doit être un entier entre 0 et 16.")?;
                Ok(())
            },
        )
    }
    pub(crate) fn set_deband_grain(&mut self, revision: &str, value: f64) -> UiUpdate {
        self.edit_renderer(revision, "setDebandGrain", "deband_grain", |values| {
            values.deband.grain = value;
            Ok(())
        })
    }
    pub(crate) fn set_interpolation(&mut self, revision: &str, enabled: bool) -> UiUpdate {
        self.edit_renderer(revision, "setInterpolation", "interpolation", |values| {
            values.interpolation = enabled;
            Ok(())
        })
    }
    fn admitted_settings(&self) -> Option<&crate::domain::state::AppliedSettings> {
        let model = self.engine.as_ref()?.model();
        let applied = model
            .active()
            .map(|active| active.applied())
            .or_else(|| model.last_valid())?;
        (applied.settings().video.identity == model.draft().settings.video.identity)
            .then_some(applied)
    }
    fn admitted_physical(&self) -> Option<capture_catalog::Physical> {
        let applied = self.admitted_settings()?;
        applied
            .physical_node()
            .map(|(node, usb)| capture_catalog::Physical {
                identity: applied.settings().video.identity.clone(),
                node: node.into(),
                usb: usb.into(),
            })
    }
    fn source_needs_catalog(&self, partial: &PartialDraft) -> bool {
        self.ui.partial.is_some() || self.ui.source_explicit || partial.source != self.ui.selected
    }
    fn profile_audio_error(&self, audio: &AudioSelection) -> Option<String> {
        if !audio.enabled() {
            return None;
        }
        if !self.ui.catalog.audio_fresh {
            return Some(format!(
                "Saved audio cannot be verified: {}",
                self.ui.catalog.audio_error
            ));
        }
        let source = audio.source().expect("enabled audio has an identity");
        let count = self
            .ui
            .catalog
            .audio
            .iter()
            .filter(|observed| source.compatible_with(&observed.identity))
            .count();
        match count {
            1 => None,
            0 => Some(format!(
                "Saved enabled audio source {:?} is absent or changed.",
                source.name()
            )),
            _ => Some(format!(
                "Saved enabled audio source {:?} is ambiguous.",
                source.name()
            )),
        }
    }
    fn commit_selection(
        &mut self,
        partial: PartialDraft,
        action: &'static str,
        field: &'static str,
        audio_reason: Option<String>,
        source_context: Option<(ProfileBinding, SelectionOrigin)>,
    ) {
        // Retain the final authoritative admission after resolving the input.
        if let Err((code, error)) = self.check_selection_ready() {
            self.ui_reject(code, action, field, error);
            return;
        }
        let physical = if action == "selectCapture" || self.source_needs_catalog(&partial) {
            match partial.source.as_ref() {
                Some(physical)
                    if self.ui.catalog.fresh && self.ui.catalog.source(physical).is_some() =>
                {
                    Some(physical.clone())
                }
                // Without a chosen source there is nothing to confirm against
                // the catalog; the completeness refusal below is the honest
                // typed outcome, not a stale-catalog diagnostic.
                Some(_) => {
                    self.ui_reject(
                        "stale_catalog",
                        action,
                        "source",
                        "Le nœud physique choisi n'est plus disponible ; actualisez le catalogue.",
                    );
                    return;
                }
                None => None,
            }
        } else {
            self.admitted_physical()
        };
        let settings = match self.settings_from_partial(&partial) {
            Ok(Some(settings)) => settings,
            Ok(None) => {
                self.ui_reject(
                    "invalid_value",
                    action,
                    field,
                    "Sélectionnez une source avec un mode complet.",
                );
                return;
            }
            Err(error) => {
                self.ui_reject("capture_error", action, field, error);
                return;
            }
        };
        let explicit_audio = matches!(action, "setAudioEnabled" | "selectAudioInput");
        if action != "selectCapture"
            && match &self.ui.partial {
                Some(current) => current == &partial,
                None => {
                    self.ui.selected == partial.source
                        && self.engine.as_ref().is_some_and(|engine| {
                            engine.model().draft().settings.same_request(&settings)
                        })
                }
            }
        {
            if explicit_audio {
                self.ui.audio_policy = crate::app::settings::StartupAudioPolicy::Explicit;
                self.ui.cli_audio = None;
                self.ui.profile_audio = false;
                self.ui.audio_reason.clear();
            }
            self.ui.error = None;
            self.dirty = true;
            return;
        }
        if let Some(engine) = &mut self.engine {
            if let Err(error) = engine.edit_draft(engine.model().draft().revision, settings) {
                self.ui_reject("busy", action, field, error.to_string());
                return;
            }
        } else {
            let Some(runner) = self.ui.bootstrap_runner.take() else {
                self.ui_reject(
                    "unavailable",
                    action,
                    field,
                    "Le propriétaire média n'est pas disponible.",
                );
                return;
            };
            self.engine = Some(ApplyCoordinator::new(
                settings,
                self.persistence.preferences().gain,
                CaptureValidator::new(),
                runner,
            ));
            self.fed_plan_revision = None;
        }
        self.ui.selected = partial.source;
        if let Some((binding, _)) = source_context {
            self.ui.selected_binding = Some((
                self.engine
                    .as_ref()
                    .expect("complete selection")
                    .model()
                    .draft()
                    .settings
                    .video
                    .identity
                    .clone(),
                binding,
            ));
        }
        self.ui.partial = None;
        if action == "selectCapture" {
            self.ui.source_explicit = true;
        }
        if explicit_audio {
            self.ui.audio_policy = crate::app::settings::StartupAudioPolicy::Explicit;
            self.ui.cli_audio = None;
            self.ui.audio_reason.clear();
        } else if action == "selectCapture" {
            self.ui.audio_reason = audio_reason.unwrap_or_default();
        }
        self.advance_edit();
        self.ui.profile_audio = if explicit_audio {
            false
        } else {
            match source_context {
                Some((_, SelectionOrigin::User { new_source })) => {
                    !new_source && self.ui.cli_audio.is_none()
                }
                Some((_, SelectionOrigin::Startup { .. })) => true,
                None => self.ui.profile_audio,
            }
        };
        if self.ui.profile_audio
            && let Some(error) = self.profile_audio_error(
                &self
                    .engine
                    .as_ref()
                    .expect("complete selection")
                    .model()
                    .draft()
                    .settings
                    .audio,
            )
        {
            self.ui_reject("audio_error", action, "audio", error);
            return;
        }
        let identity = &self
            .engine
            .as_ref()
            .expect("complete selection")
            .model()
            .draft()
            .settings
            .video
            .identity;
        let binding = source_context
            .map(|(binding, _)| binding)
            .or_else(|| {
                self.ui
                    .selected_binding
                    .as_ref()
                    .filter(|(selected, _)| selected == identity)
                    .map(|(_, binding)| *binding)
            })
            .unwrap_or_else(|| {
                self.persistence
                    .profile_binding(identity, self.serial_unique(identity))
            });
        let engine = self.engine.as_mut().expect("complete selection");
        let submitted = engine.model().draft().clone();
        let result = match physical.as_ref() {
            Some(physical) => engine.apply_selected_capture(
                engine.model().state_identity(),
                submitted.revision,
                (&physical.node, &physical.usb),
            ),
            None => engine.apply(engine.model().state_identity(), submitted.revision),
        };
        match result {
            Ok(admission) => {
                self.ui.submitted = Some((admission.id(), self.ui.selected.clone()));
                if self.ui.profile_audio
                    && matches!(admission, crate::domain::state::ApplyAdmission::Open { .. })
                    && !matches!(source_context, Some((_, SelectionOrigin::Startup { .. })))
                    && let Err(error) = engine.require_profile_restore_audio(admission.id())
                {
                    self.ui_reject("audio_error", action, "audio", error.to_string());
                    return;
                }
                match source_context {
                    Some((_, SelectionOrigin::Startup { original })) => {
                        self.startup_apply = Some(admission.id());
                        if let Err(error) = engine.require_startup_restore_audio(admission.id()) {
                            let reason = format!("Initial restoration guard failed: {error}");
                            if engine.close(engine.model().state_identity()).is_err() {
                                engine.quit();
                            }
                            self.persistence.startup_failed(reason.clone());
                            self.rejection(reason);
                            return;
                        }
                        if submitted.settings.video.mode != original {
                            self.persistence.admit_startup_adaptation(
                                admission,
                                submitted,
                                binding,
                                self.ui.audio_policy,
                            );
                        }
                    }
                    _ => self
                        .persistence
                        .admit_user_apply(admission, submitted, binding),
                }
                self.ui.error = None;
                self.command_error.clear();
                self.dirty = true;
            }
            Err(error) => self.ui_reject("busy", action, field, error.to_string()),
        }
    }
    fn sync_ui_tokens(&mut self) {
        let identity = self
            .engine
            .as_ref()
            .map(|engine| engine.model().state_identity());
        if !self.ui.state_initialized {
            self.ui.state_identity = identity;
            self.ui.state_initialized = true;
            self.dirty = true;
        } else if self.ui.state_identity != identity {
            match self.ui.state_sequence.checked_add(1) {
                Some(sequence) => self.ui.state_sequence = sequence,
                None => self.ui.exhausted = true,
            }
            self.ui.state_identity = identity;
            self.dirty = true;
        }
        let revision = self
            .engine
            .as_ref()
            .map(|engine| engine.model().draft().revision);
        if self.ui.domain_revision != revision {
            if self.ui.snapshot.is_some() {
                match self.ui.public_revision.checked_add(1) {
                    Some(sequence) => self.ui.public_revision = sequence,
                    None => self.ui.exhausted = true,
                }
            }
            self.ui.domain_revision = revision;
            self.dirty = true;
        }
        let can_inherit = !self.ui.source_explicit && self.ui.partial.is_none();
        let admitted_changed = can_inherit
            && self.admitted_settings().is_some_and(|applied| {
                applied.physical_node().is_some_and(|(node, usb)| {
                    self.ui.selected.as_ref().is_none_or(|source| {
                        source.identity != applied.settings().video.identity
                            || source.node.as_path() != node
                            || source.usb.as_path() != usb
                    })
                })
            });
        if admitted_changed && let Some(physical) = self.admitted_physical() {
            self.ui.selected = Some(physical);
            self.dirty = true;
        }
        if self.ui.selected.is_none() && self.ui.partial.is_none() {
            let physical = self.admitted_physical().or_else(|| {
                self.engine
                    .as_ref()
                    .and_then(|engine| {
                        self.ui
                            .catalog
                            .source_for_video(&engine.model().draft().settings.video)
                    })
                    .map(|source| source.physical())
            });
            if let Some(physical) = physical {
                self.ui.selected = Some(physical);
                self.dirty = true;
            }
        }
    }
    fn selection_projection(
        &self,
        partial: &PartialDraft,
        settings: Option<&DraftSettings>,
    ) -> projection::Selection {
        let physical = partial.source.as_ref();
        let source = physical.and_then(|physical| self.ui.catalog.source(physical));
        let fallback = settings
            .map(|settings| {
                format!(
                    "USB {:04x}:{:04x} · {}",
                    settings.video.identity.vendor_id(),
                    settings.video.identity.product_id(),
                    capture_catalog::topology(&settings.video.identity)
                )
            })
            .unwrap_or_else(|| "Aucune source sélectionnée".into());
        let manual = match partial.policy {
            InputFormatPolicy::Auto => None,
            InputFormatPolicy::Manual { fourcc } => Some(fourcc),
        };
        projection::Selection {
            source_key: physical
                .map(|physical| self.ui.catalog.source_key(physical))
                .unwrap_or_default(),
            source_label: source
                .map(|source| source.node.card_name().to_owned())
                .unwrap_or(fallback),
            source_available: source.is_some() && self.ui.catalog.fresh,
            size: partial.size.map(Into::into),
            rate: partial.rate.map(Into::into),
            format_policy: if manual.is_some() { "manual" } else { "auto" },
            manual_format_key: manual
                .map(|fourcc| self.ui.catalog.format_key(fourcc))
                .unwrap_or_default(),
            requested_format_label: manual
                .map(capture_catalog::fourcc_text)
                .unwrap_or_else(|| "Auto".into()),
            resolved_format_label: settings
                .and_then(|settings| settings.video.resolved_fourcc)
                .map(capture_catalog::fourcc_text)
                .unwrap_or_else(|| "Non préparé".into()),
            audio_enabled: partial.audio.enabled(),
            audio_input_key: self.ui.catalog.input_key(&partial.audio),
            audio_input_label: self.audio_input_label(&partial.audio),
            filters: partial.filters.clone(),
            renderer: partial.renderer.clone(),
        }
    }
    fn audio_input_label(&self, audio: &AudioSelection) -> String {
        audio
            .source()
            .map(|identity| {
                self.ui
                    .catalog
                    .audio
                    .iter()
                    .find(|source| identity.compatible_with(&source.identity))
                    .filter(|source| !source.description.is_empty())
                    .map(|source| source.description.clone())
                    .unwrap_or_else(|| identity.name().to_owned())
            })
            .unwrap_or_default()
    }
    fn stream_stats(&self, settings: &DraftSettings) -> projection::StreamStats {
        let identity = &settings.video.identity;
        let ports = identity
            .topology()
            .ports()
            .iter()
            .map(|port| port.get().to_string())
            .collect::<Vec<_>>()
            .join(".");
        projection::StreamStats {
            identity_text: format!(
                "USB {:04x}:{:04x}\nController {} / ports {} / serial {}",
                identity.vendor_id(),
                identity.product_id(),
                identity.topology().controller(),
                ports,
                identity.serial().unwrap_or("none")
            ),
            input_policy: match settings.video.mode.format_policy {
                InputFormatPolicy::Auto => "Auto".into(),
                InputFormatPolicy::Manual { fourcc } => {
                    format!("Manual {}", capture_catalog::fourcc_text(fourcc))
                }
            },
            size: settings.video.mode.size.into(),
            rate: settings.video.mode.rate.into(),
            resolved_input: format!(
                "{} (advertised selection, not negotiated proof)",
                settings
                    .video
                    .resolved_fourcc
                    .map(capture_catalog::fourcc_text)
                    .unwrap_or_else(|| "not prepared".into())
            ),
            audio_enabled: settings.audio.enabled(),
            audio_input_label: self.audio_input_label(&settings.audio),
        }
    }
    fn applied_projection(
        &self,
        applied: &crate::domain::state::AppliedSettings,
    ) -> projection::Selection {
        let settings = applied.settings();
        let source = applied
            .physical_node()
            .map(|(node, usb)| capture_catalog::Physical {
                identity: settings.video.identity.clone(),
                node: node.into(),
                usb: usb.into(),
            });
        let partial = PartialDraft {
            source,
            size: Some(settings.video.mode.size),
            rate: Some(settings.video.mode.rate),
            policy: settings.video.mode.format_policy,
            audio: settings.audio.clone(),
            filters: settings.filters.clone(),
            renderer: settings.renderer.clone(),
        };
        self.selection_projection(&partial, Some(settings))
    }
    fn product_snapshot(
        &mut self,
        update: &UiUpdate,
        output: &OutputProjection,
    ) -> projection::Snapshot {
        let partial = self.current_partial();
        self.ui.catalog.rebuild(
            partial.source.as_ref(),
            partial.policy,
            partial.size,
            partial.rate,
            &partial.audio,
        );
        let model = self.engine.as_ref().map(|engine| engine.model());
        let draft_settings = if self.ui.partial.is_none() {
            model.map(|model| &model.draft().settings)
        } else {
            None
        };
        let draft = (partial.size.is_some()
            && partial.rate.is_some()
            && (draft_settings.is_some() || partial.source.is_some()))
        .then(|| self.selection_projection(&partial, draft_settings));
        let applied = model
            .and_then(|model| model.active())
            .map(|active| self.applied_projection(active.applied()));
        let last_valid = model
            .and_then(ProductModel::last_valid)
            .map(|applied| self.applied_projection(applied));
        let mut filters = self.ui.filters.clone();
        filters.entries = partial
            .filters
            .entries()
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let available = filters
                    .kinds
                    .iter()
                    .find(|kind| kind.kind == entry.kind().as_str());
                projection::FilterEntryRow {
                    key: format!("filter:{}:{index}", self.ui.public_revision),
                    label: entry.label().into(),
                    kind: entry.kind().as_str(),
                    enabled: entry.enabled(),
                    parameters: projection::Parameters::from(entry.filter()),
                    available: available.is_some_and(|kind| kind.available),
                    reason: available
                        .map(|kind| kind.reason.clone())
                        .unwrap_or_else(|| "Registre qualifié indisponible".into()),
                }
            })
            .collect();
        let old_live = self.ui.snapshot.as_ref().map(|snapshot| &snapshot.live);
        let visible = applied.as_ref().or(last_valid.as_ref());
        let live = projection::Live {
            source_label: visible
                .map(|selection| selection.source_label.clone())
                .unwrap_or_else(|| "Aucune capture active".into()),
            size_label: visible
                .and_then(|selection| selection.size)
                .map(|size| format!("{} × {}", size.width, size.height))
                .unwrap_or_else(|| "Résolution".into()),
            rate_label: visible
                .and_then(|selection| selection.rate.as_ref())
                .map(|rate| rate.text.clone())
                .unwrap_or_else(|| "Framerate".into()),
            format_label: visible
                .map(|selection| selection.resolved_format_label.clone())
                .unwrap_or_default(),
            audio_status: if update.audio_status.is_empty() {
                old_live
                    .map(|live| live.audio_status.clone())
                    .unwrap_or_default()
            } else {
                update.audio_status.clone()
            },
            audio_diagnostic: if !self.ui.audio_reason.is_empty() {
                self.ui.audio_reason.clone()
            } else if !update.audio_status.is_empty() {
                update.audio_diagnostic.clone()
            } else {
                old_live
                    .map(|live| live.audio_diagnostic.clone())
                    .unwrap_or_default()
            },
            playback_status: if update.playback_status.is_empty() {
                old_live
                    .map(|live| live.playback_status.clone())
                    .unwrap_or_else(|| "Unavailable".into())
            } else {
                update.playback_status.clone()
            },
            volume_percent: update.volume_percent,
            muted: update.muted,
        };
        let recovery = projection::Recovery {
            evidence: model
                .and_then(ProductModel::recovery)
                .map(|recovery| loss_evidence_text(&recovery.evidence))
                .unwrap_or_default(),
            stage: model
                .and_then(ProductModel::recovery)
                .map(|recovery| recovery.failure.to_string())
                .unwrap_or_default(),
            candidates: model
                .map(|model| {
                    model
                        .recovery_candidates()
                        .iter()
                        .map(|candidate| projection::RecoveryRow {
                            key: format!(
                                "{}:{}:{}",
                                candidate.token.stamp.watch.get(),
                                candidate.token.stamp.epoch.get(),
                                candidate.token.candidate.get()
                            ),
                            label: candidate.description.clone(),
                            detail: format!(
                                "USB {:04x}:{:04x} · {}",
                                candidate.identity.vendor_id(),
                                candidate.identity.product_id(),
                                capture_catalog::topology(&candidate.identity)
                            ),
                            eligible: self.persistence.mutations_open(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        projection::Snapshot {
            schema: 2,
            state_token: format!("state:{}", self.ui.state_sequence),
            generation: update.restart_generation.to_string(),
            product_phase: model
                .map(|model| phase_name(model.phase()).into())
                .unwrap_or_else(|| "Idle".into()),
            gate_phase: format!("{:?}", update.phase),
            paused: update.paused,
            opening: matches!(update.phase, GatePhase::WaitingSurface | GatePhase::Opening),
            closing: !self.persistence.mutations_open(),
            can_edit_selection: self.check_selection_ready().is_ok(),
            can_restart: self.persistence.mutations_open()
                && model.is_some_and(ProductModel::can_restart),
            can_reconnect: self.persistence.mutations_open()
                && model.is_some_and(ProductModel::can_reconnect),
            can_toggle_pause: update.can_toggle_pause,
            can_set_gain: update.can_set_gain,
            selection_revision: self.ui.public_revision.to_string(),
            selection: draft,
            applied,
            last_valid,
            live,
            stream_stats: model.map(|model| self.stream_stats(stream_settings(model))),
            catalog: self.ui.catalog.projection.clone(),
            filters,
            renderer: self.ui.renderer.clone(),
            output: projection::Output {
                revision: output.catalog_revision.to_string(),
                rows: self
                    .last_rows
                    .iter()
                    .map(|row| projection::OutputRow {
                        key: row.key.clone(),
                        label: row.label.clone(),
                        name: row.name.clone(),
                        eligible: row.eligible,
                        reason: if row.eligible {
                            String::new()
                        } else {
                            "Sortie indisponible".into()
                        },
                    })
                    .collect(),
                selected_key: output.selected_key.clone(),
                selected_label: self
                    .last_rows
                    .iter()
                    .find(|row| row.key == output.selected_key)
                    .map(|row| row.label.clone())
                    .unwrap_or_else(|| output.selected.clone()),
                effective_label: self
                    .last_rows
                    .iter()
                    .find(|row| row.name == output.effective)
                    .map(|row| row.label.clone())
                    .unwrap_or_else(|| output.effective.clone()),
                status: output.status.clone(),
                needs_action: output.needs_action,
            },
            recovery,
            settings: projection::Settings {
                status: self.persistence.status(),
                path: self.persistence.path(),
                refused: self.persistence.refused(),
                startup_reason: self.persistence.startup_reason().into(),
                close_dialog: self.persistence.close_dialog().into(),
                reset_token: self.persistence.reset_token().to_string(),
            },
            intent_error: self.ui.error.clone(),
        }
    }
    pub(crate) fn new(
        prefix: String,
        treatment_capabilities: crate::media::render_catalog::QualifiedTreatmentCapabilities,
        startup: StartupSelection,
        persistence: PersistenceSession,
        sources: Vec<AudioSourceIdentity>,
    ) -> Self {
        let gain = persistence.preferences().gain;
        let output = OutputPolicy::new(persistence.preferences().output.clone());
        let selected_binding = startup
            .source_binding
            .zip(
                startup
                    .draft
                    .as_ref()
                    .map(|settings| settings.video.identity.clone()),
            )
            .map(|(binding, identity)| (identity, binding));
        let (catalog, catalog_start_failed) = match OutputCatalogWatch::start() {
            Ok(watch) => (Some(watch), false),
            Err(error) => {
                tracing::warn!(error = %error, "output catalog watch unavailable");
                (None, true)
            }
        };
        let (filters, renderer) = projection::treatments(&treatment_capabilities);
        let runner = GateRunner::new(prefix, treatment_capabilities);
        let cli_audio = (startup.audio_policy == crate::app::settings::StartupAudioPolicy::Cli)
            .then(|| {
                startup
                    .draft
                    .as_ref()
                    .map(|settings| settings.audio.clone())
                    .unwrap_or_else(|| startup.bootstrap_audio.clone())
            });
        let (engine, bootstrap_runner, partial) = match startup.draft {
            Some(settings) => (
                Some(ApplyCoordinator::new(
                    settings,
                    gain,
                    CaptureValidator::new(),
                    runner,
                )),
                None,
                None,
            ),
            None => (
                None,
                Some(runner),
                Some(PartialDraft {
                    audio: startup.bootstrap_audio,
                    ..PartialDraft::default()
                }),
            ),
        };
        Self {
            engine,
            ui: ProductUi {
                audio_policy: startup.audio_policy,
                selected_binding,
                cli_audio,
                profile_audio: startup.restore_profile
                    && startup.audio_policy != crate::app::settings::StartupAudioPolicy::Cli,
                startup_adaptation: startup.adapt_video,
                startup_pin: startup.topology_pin,
                filters,
                renderer,
                bootstrap_runner,
                partial,
                ..ProductUi::default()
            },
            sources,
            dirty: true,
            last_state: None,
            command_error: String::new(),
            quit_empty: false,
            auto_open: startup.auto_open,
            startup_apply: None,
            persistence,
            output,
            catalog,
            catalog_start_failed,
            registered_target: None,
            fed_plan_revision: None,
            last_catalog: None,
            last_rows: Vec::new(),
            last_output: None,
            catalog_stopping: false,
            catalog_join_error: None,
        }
    }
    pub(crate) fn ui_ready(&mut self) -> UiUpdate {
        self.ui.catalog.start();
        if std::mem::take(&mut self.auto_open)
            && self.persistence.mutations_open()
            && let Some(engine) = &mut self.engine
        {
            let pin = self.ui.startup_pin.take();
            if self
                .ui
                .selected_binding
                .as_ref()
                .is_some_and(|(_, binding)| *binding == ProfileBinding::Topology)
                && pin.is_none()
            {
                self.persistence
                    .startup_failed("Topology-bound startup has no fresh preflight pin.".into());
                self.dirty = true;
                return self.poll();
            }
            let admission = match pin.as_ref() {
                Some((node, usb)) => engine.apply_selected_capture(
                    engine.model().state_identity(),
                    engine.model().draft().revision,
                    (node, usb),
                ),
                None => engine.apply(
                    engine.model().state_identity(),
                    engine.model().draft().revision,
                ),
            };
            match admission {
                Ok(admission) => {
                    let apply = admission.id();
                    self.startup_apply = Some(apply);
                    if let Err(error) = engine.require_startup_restore_audio(apply) {
                        let mut reason = format!("Initial restoration guard failed: {error}");
                        if let Err(close_error) = engine.close(engine.model().state_identity()) {
                            reason.push_str(&format!("; cleanup admission failed: {close_error}"));
                            engine.quit();
                        }
                        self.persistence.startup_failed(reason.clone());
                        self.rejection(reason);
                    }
                }
                Err(error) => {
                    self.persistence.startup_failed(error.to_string());
                    self.rejection(error);
                }
            }
        }
        self.dirty = true;
        self.poll()
    }
    fn begin_shutdown(&mut self) {
        if self.ui.resolving.take().is_some() {
            self.ui_reject(
                "closing",
                "selectCapture",
                "source",
                "La fermeture annule la résolution de la source.",
            );
        }
        self.ui.catalog.cancel_mode();
        // The catalog worker must stop before the Qt loop exits: request the
        // stop here, then let poll() drain its non-blocking try_join until
        // the worker is actually complete. Drop only guarantees the final
        // join fallback after Qt exit, never the quit barrier itself.
        self.catalog_stopping = true;
        if let Some(worker) = &self.ui.catalog.worker {
            worker.stop();
        }
        if let Some(apply) = self.persistence.authorized_apply() {
            self.persistence.cancel_user_apply(apply);
        }
        if let Some(watch) = &mut self.catalog {
            watch.stop();
        }
        if let Some(engine) = &mut self.engine {
            engine.quit();
        } else {
            self.quit_empty = true;
        }
        self.dirty = true;
    }
    /// Shuts the catalog worker down toward completion without blocking the
    /// GUI thread. Returns true only once the worker is fully joined (or
    /// never started / failed to start); a terminal join failure is recorded
    /// and clears the watch so the barrier still opens.
    fn drain_catalog(&mut self) -> bool {
        if self.catalog.is_none() {
            return true;
        }
        if !self.catalog_stopping {
            self.catalog_stopping = true;
            if let Some(watch) = &mut self.catalog {
                watch.stop();
            }
        }
        let mut joined = false;
        if let Some(watch) = &mut self.catalog
            && let Some(result) = watch.try_join()
        {
            if let Err(error) = result {
                self.catalog_join_error = Some(error.to_string());
                self.rejection(format!("output catalog watch failed to stop: {error}"));
            }
            joined = true;
        }
        if joined {
            self.catalog = None;
        }
        joined
    }
    pub(crate) fn request_application_close(&mut self) -> UiUpdate {
        if self.persistence.request_close() {
            self.begin_shutdown();
        }
        self.dirty = true;
        self.poll()
    }
    pub(crate) fn retry_save(&mut self) -> UiUpdate {
        self.persistence.retry_close_save();
        self.dirty = true;
        self.poll()
    }
    pub(crate) fn close_without_save(&mut self) -> UiUpdate {
        self.persistence.close_without_save();
        self.dirty = true;
        self.poll()
    }
    pub(crate) fn request_reset(&mut self) -> UiUpdate {
        self.ui.startup_adaptation = false;
        self.ui.resolving = None;
        self.ui.catalog.cancel_mode();
        self.persistence.request_reset();
        self.dirty = true;
        self.poll()
    }
    pub(crate) fn decide_reset(&mut self, token: u64, confirmed: bool) -> UiUpdate {
        self.persistence.decide_reset(token, confirmed);
        self.dirty = true;
        self.poll()
    }
    pub(crate) fn set_fullscreen(&mut self, fullscreen: bool) -> UiUpdate {
        if self.persistence.mutations_open() {
            self.persistence.set_fullscreen_preference(fullscreen);
        }
        self.dirty = true;
        self.poll()
    }
    pub(crate) fn set_stats_hidden(&mut self, hidden: bool) -> UiUpdate {
        if self.persistence.mutations_open() {
            self.persistence.set_stats_hidden_preference(hidden);
        }
        self.dirty = true;
        self.poll()
    }
    fn rejection(&mut self, error: impl std::fmt::Display) {
        self.command_error = error.to_string();
        self.dirty = true;
        tracing::warn!(diagnostic = %serde_json::json!({"error": self.command_error}), "qualification_command_rejected");
    }
    fn qualification_rejection(&mut self, line: &str, error: impl std::fmt::Display) {
        // Serialize before tracing or projecting: inert user labels can contain
        // controls, but must never turn one rejected request into log lines.
        self.rejection(serde_json::json!({"input": line, "error": error.to_string()}));
    }
    /// Open is a plain draft apply. It must never silently reconnect a lost
    /// session: reconnection is the separate explicit idempotent admission in
    /// [`Self::reconnect`], and `restart` stays the explicit forced restart.
    pub(crate) fn open(&mut self) -> UiUpdate {
        if !self.persistence.mutations_open() {
            return self.poll();
        }
        self.cancel_startup_selection();
        if self.ui.resolving.is_some() {
            self.ui_reject(
                "busy",
                "openCapture",
                "capture",
                "Le mode de la source choisie est en cours de résolution.",
            );
            return self.poll();
        }
        self.observe_profile_snapshot();
        if self.ui.profile_audio
            && let Some(error) = self
                .engine
                .as_ref()
                .and_then(|engine| self.profile_audio_error(&engine.model().draft().settings.audio))
        {
            self.ui_reject("audio_error", "openCapture", "audio", error);
            return self.poll();
        }
        let binding = self.engine.as_ref().map(|engine| {
            let identity = &engine.model().draft().settings.video.identity;
            self.ui
                .selected_binding
                .as_ref()
                .filter(|(selected, _)| selected == identity)
                .map(|(_, binding)| *binding)
                .unwrap_or_else(|| {
                    self.persistence
                        .profile_binding(identity, self.serial_unique(identity))
                })
        });
        if let Some(engine) = &mut self.engine {
            let submitted = engine.model().draft().clone();
            let result = engine.apply(engine.model().state_identity(), submitted.revision);
            match result {
                Err(error) => self.rejection(error),
                Ok(apply) => {
                    if self.ui.profile_audio
                        && matches!(apply, crate::domain::state::ApplyAdmission::Open { .. })
                        && let Err(error) = engine.require_profile_restore_audio(apply.id())
                    {
                        self.rejection(error);
                        return self.poll();
                    }
                    self.persistence.admit_user_apply(
                        apply,
                        submitted,
                        binding.expect("engine binding"),
                    );
                    self.command_error.clear();
                    self.dirty = true;
                }
            }
        }
        self.poll()
    }
    pub(crate) fn restart(&mut self, expected_attempt: u64) -> UiUpdate {
        if !self.persistence.mutations_open() {
            return self.poll();
        }
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
        if !self.persistence.mutations_open() {
            return self.poll();
        }
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
                    if let Some(apply) = self.persistence.authorized_apply() {
                        self.persistence.cancel_user_apply(apply);
                    }
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
        if !self.persistence.mutations_open() {
            return self.poll();
        }
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
        if !self.persistence.mutations_open() {
            return self.poll();
        }
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
    /// Uncontrolled root/event-loop loss drains without unavailable UI dialogs.
    /// Normal window close uses request_application_close.
    pub(crate) fn quit(&mut self) -> UiUpdate {
        self.persistence.force_close();
        self.begin_shutdown();
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
        if !self.persistence.mutations_open() {
            return SubmitStatus::Closing;
        }
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
        let current = self.persistence.preferences().gain;
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
        self.submit_gain(PlaybackGain {
            muted,
            ..self.persistence.preferences().gain
        })
    }
    fn submit_gain(&mut self, gain: PlaybackGain) -> SubmitStatus {
        if !self.persistence.mutations_open() {
            self.rejection("gain submission Closing");
            return SubmitStatus::Closing;
        }
        let status = match self.engine.as_mut() {
            Some(engine) => engine.set_gain(gain),
            None => SubmitStatus::Accepted,
        };
        if status == SubmitStatus::Accepted {
            self.persistence.set_gain_preference(gain);
            self.command_error.clear();
        } else {
            self.rejection(format!("gain submission {status:?}"));
        }
        status
    }
    /// Advances the application-owned output routing: consumes catalog
    /// observations into the policy, feeds a changed desired plan to the
    /// engine and registers a changed live target with the watch. The last
    /// registered target is retained through silence and catalog errors; only
    /// an actual target change (or an explicit selection in
    /// [`Self::select_output`]) re-registers and resets removal evidence.
    fn pump_output(&mut self) {
        if self.catalog.is_none() && !self.catalog_start_failed && !self.catalog_stopping {
            match OutputCatalogWatch::start() {
                Ok(watch) => self.catalog = Some(watch),
                Err(error) => {
                    self.catalog_start_failed = true;
                    self.rejection(format!("output catalog watch unavailable: {error}"));
                    return;
                }
            }
        }
        let mut observed = false;
        if let Some(watch) = &self.catalog {
            while let Some(observation) = watch.poll() {
                if let Ok(catalog) = &observation.catalog {
                    self.last_catalog = Some(catalog.clone());
                }
                self.output
                    .observe(observation.catalog, observation.selected_target_removed);
                observed = true;
            }
        }
        if observed || self.fed_plan_revision != Some(self.output.revision()) {
            let plan = self.output.plan().clone();
            if let Some(engine) = &mut self.engine {
                match engine.set_output_plan(plan.clone()) {
                    Ok(SubmitStatus::Accepted) => {}
                    Ok(status) => self.rejection(format!("output plan submission {status:?}")),
                    Err(error) => self.rejection(error),
                }
            }
            if let Some(target) = plan.target()
                && self.registered_target.as_ref() != Some(target)
            {
                if let Some(watch) = &self.catalog {
                    watch.set_selected_target(Some(target.clone()));
                }
                self.registered_target = Some(target.clone());
            }
            self.fed_plan_revision = Some(self.output.revision());
            self.dirty = true;
        }
    }

    /// Explicit output choice from the selector. `row_key` is the reserved
    /// auto key or an opaque revision-scoped row key from the currently
    /// displayed rows; `expected_catalog_revision` must still match the rows
    /// the user chose from, so a stale dialog can never apply a decision
    /// about a catalog that has since changed. The candidate policy is
    /// staged on a clone and committed only on engine acceptance: a rejected
    /// selection leaves the latch, choice and desired revision untouched.
    pub(crate) fn select_output(
        &mut self,
        row_key: &str,
        expected_catalog_revision: u64,
    ) -> SubmitStatus {
        self.dirty = true;
        if !self.persistence.mutations_open() {
            self.rejection("output selection while closing");
            return SubmitStatus::Closing;
        }
        let catalog_revision = self.last_catalog.as_ref().map(|c| c.revision).unwrap_or(0);
        if catalog_revision != expected_catalog_revision {
            self.rejection("stale output selection: the catalog changed; choose again");
            return SubmitStatus::StaleGeneration;
        }
        let identity = if row_key == AUTO_ROW_KEY {
            None
        } else {
            match self
                .last_rows
                .iter()
                .find(|row| row.key == row_key)
                .and_then(|row| row.identity.clone())
            {
                Some(identity) => Some(identity),
                None => {
                    self.rejection("selected output is not in the current catalog");
                    return SubmitStatus::NotReady;
                }
            }
        };
        let choice = match identity {
            None => PersistentOutputChoice::Auto,
            Some(identity) => PersistentOutputChoice::Manual(identity),
        };
        // Stage on a clone: nothing observable changes until the engine
        // accepts the candidate plan.
        let mut candidate = self.output.clone();
        candidate.select(choice.clone());
        let plan = candidate.plan().clone();
        let status = match self.engine.as_mut() {
            Some(engine) => match engine.set_output_plan(plan.clone()) {
                Ok(status) => status,
                Err(error) => {
                    // The engine's actual diagnosis is preserved; only the
                    // coarse SubmitStatus is mapped for the caller.
                    self.rejection(error);
                    return SubmitStatus::NotReady;
                }
            },
            None => SubmitStatus::Accepted,
        };
        if status != SubmitStatus::Accepted {
            self.rejection(format!("output selection {status:?}"));
            return status;
        }
        self.output = candidate;
        self.command_error.clear();
        let mut preferences = self.persistence.preferences().clone();
        preferences.output = choice;
        self.persistence.set_preferences(preferences);
        // An explicit selection always re-registers, including the same
        // target: fresh selection must not inherit stale removal evidence.
        if let Some(watch) = &self.catalog {
            watch.set_selected_target(plan.target().cloned());
        }
        self.registered_target = plan.target().cloned();
        self.fed_plan_revision = Some(self.output.revision());
        status
    }

    /// Legacy selector values built from the last observed catalog.
    /// Rows are a JSON array; action keys never derive from sink names.
    fn output_projection(&mut self) -> OutputProjection {
        let plan = self.output.plan();
        let catalog_revision = self.last_catalog.as_ref().map(|c| c.revision).unwrap_or(0);
        let (selected, selected_key) = match self.output.choice() {
            PersistentOutputChoice::Auto => ("auto".to_owned(), AUTO_ROW_KEY.to_owned()),
            PersistentOutputChoice::Manual(identity) => {
                // Display keeps the chosen name; the action key is the key of
                // the UNIQUE compatible observed sink, and stays empty when
                // the choice is absent, renamed or ambiguous.
                let key = self.last_catalog.as_ref().and_then(|catalog| {
                    let matches: Vec<usize> = catalog
                        .sinks
                        .iter()
                        .enumerate()
                        .filter(|(_, observation)| {
                            identity.compatible_with(&observation.target.identity)
                        })
                        .map(|(index, _)| index)
                        .collect();
                    match matches[..] {
                        [index] => Some(format!("sink:{catalog_revision}:{index}")),
                        _ => None,
                    }
                });
                (identity.name().to_owned(), key.unwrap_or_default())
            }
        };
        // The effective output and its confirmation status come from the
        // media-confirmed availability (the Active receipt's actual
        // destination), never from the desired plan.
        let availability = self
            .engine
            .as_ref()
            .and_then(|engine| engine.audio_availability().cloned());
        let effective = match &availability {
            Some(AudioAvailability::Active { route, .. }) => route
                .destination()
                .map(|target| target.identity.name().to_owned())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let status = match &availability {
            Some(AudioAvailability::Silent {
                reason: AudioSilence::Output(reason),
            }) => output_silence_text(reason),
            Some(AudioAvailability::Switching { .. }) => "Switching output route".to_owned(),
            _ => match plan.silence() {
                None => String::new(),
                Some(reason) => output_silence_text(reason),
            },
        };
        // Rows are serialized as a JSON array of objects: sink names and
        // descriptions are external strings and must never ride as raw
        // newline/pipe text. Keys are opaque and revision-scoped; the
        // reserved Auto key can never collide with a catalogued sink.
        let mut rows = vec![OutputRow {
            key: AUTO_ROW_KEY.to_owned(),
            label: "Auto".to_owned(),
            name: AUTO_ROW_KEY.to_owned(),
            eligible: true,
            identity: None,
        }];
        if let Some(catalog) = &self.last_catalog {
            for (index, sink) in catalog.sinks.iter().enumerate() {
                let name = sink.target.identity.name().to_owned();
                let label = if sink.description.is_empty() {
                    name.clone()
                } else {
                    sink.description.clone()
                };
                rows.push(OutputRow {
                    key: format!("sink:{catalog_revision}:{index}"),
                    label,
                    name,
                    eligible: sink.eligible,
                    identity: Some(sink.target.identity.clone()),
                });
            }
        }
        self.last_rows = rows.clone();
        let serialized = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_owned());
        OutputProjection {
            catalog_revision,
            rows: serialized,
            selected,
            selected_key,
            effective,
            status,
            needs_action: matches!(plan.silence(), Some(OutputSilence::ManualRequiresAction)),
        }
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
            || identity.treatment_pass() != expected.treatment_pass
        {
            Err(CommandRejection::StaleState)
        } else {
            Ok(identity)
        }
    }
    pub(crate) fn qualification_command(&mut self, line: &str) -> UiUpdate {
        // Opt-in qualification calls the same production application decisions.
        if !control::is_structured_json_command(line) {
            if line.len() > control::LEGACY_COMMAND_MAX_BYTES {
                self.qualification_rejection(line, "qualification command exceeds 256 bytes");
                return self.poll();
            }
            if line
                .chars()
                .any(|value| value == '\0' || (value.is_whitespace() && !value.is_ascii()))
            {
                self.qualification_rejection(line, "NUL or non-ASCII whitespace forbidden");
                return self.poll();
            }
        }
        tracing::info!(request = %serde_json::json!({"input": line}), "qualification_request_received");
        match line.trim() {
            "settings" => {
                self.dirty = true;
                return self.poll();
            }
            "application-close" => return self.request_application_close(),
            "save-retry" => return self.retry_save(),
            "close-without-save" => return self.close_without_save(),
            "settings-reset" => return self.request_reset(),
            "reset-cancel" => return self.decide_reset(self.persistence.reset_token(), false),
            "reset-confirm" => return self.decide_reset(self.persistence.reset_token(), true),
            _ => {}
        }
        if let Some(value) = line.strip_prefix("preference-volume ") {
            match value.parse::<i32>() {
                Ok(value) => {
                    self.set_volume(value);
                }
                Err(_) => {
                    self.qualification_rejection(line, "preference-volume requires an integer")
                }
            }
            return self.poll();
        }
        if let Some(value) = line.strip_prefix("preference-mute ") {
            match value {
                "true" => {
                    self.set_muted(true);
                }
                "false" => {
                    self.set_muted(false);
                }
                _ => self.qualification_rejection(line, "preference-mute requires true or false"),
            }
            return self.poll();
        }
        let command = match control::parse(line) {
            Ok(command) => command,
            Err(error) => {
                self.qualification_rejection(line, error);
                return self.poll();
            }
        };
        tracing::info!(request = %serde_json::json!({"input": line, "verb": line.split_ascii_whitespace().next()}), "qualification_request_parsed");
        if command == Command::Snapshot {
            self.dirty = true;
            return self.poll();
        }
        if !self.persistence.mutations_open() {
            self.qualification_rejection(
                line,
                "application is closing; capture/draft mutations are blocked",
            );
            return self.poll();
        }
        self.cancel_startup_selection();
        self.observe_profile_snapshot();
        let binding = self.engine.as_ref().map(|engine| {
            let identity = &engine.model().draft().settings.video.identity;
            self.ui
                .selected_binding
                .as_ref()
                .filter(|(selected, _)| selected == identity)
                .map(|(_, binding)| *binding)
                .unwrap_or_else(|| {
                    self.persistence
                        .profile_binding(identity, self.serial_unique(identity))
                })
        });
        let restored_audio_error = self
            .ui
            .profile_audio
            .then(|| {
                self.engine.as_ref().and_then(|engine| {
                    self.profile_audio_error(&engine.model().draft().settings.audio)
                })
            })
            .flatten();
        let Some(engine) = &mut self.engine else {
            self.qualification_rejection(line, "no explicit startup capture selection");
            return self.poll();
        };
        let result: Result<(), String> = (|| {
            match command {
                Command::Filters(revision, filters) => {
                    let mut settings = engine.model().draft().settings.clone();
                    settings.filters = filters;
                    engine
                        .edit_draft(revision, settings)
                        .map_err(|error| error.to_string())?;
                }
                Command::Renderer(revision, renderer) => {
                    // Editing stores the requested renderer only; nothing is
                    // applied until an explicit Apply/Open.
                    let mut settings = engine.model().draft().settings.clone();
                    settings.renderer = renderer;
                    engine
                        .edit_draft(revision, settings)
                        .map_err(|error| error.to_string())?;
                }
                Command::Video(revision, mode) => {
                    let mut settings = engine.model().draft().settings.clone();
                    settings.video.mode = mode;
                    settings.video.resolved_fourcc = None;
                    engine
                        .edit_draft(revision, settings)
                        .map_err(|error| error.to_string())?;
                }
                Command::Identity(revision, identity) => {
                    let mut settings = engine.model().draft().settings.clone();
                    settings.video.identity = identity;
                    settings.video.resolved_fourcc = None;
                    settings.video.selected_node = None;
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
                    if let Some(error) = &restored_audio_error {
                        return Err(error.clone());
                    }
                    let state =
                        Self::checked_state(engine, expected).map_err(|error| error.to_string())?;
                    let submitted = engine.model().draft().clone();
                    let apply = engine
                        .apply(state, revision)
                        .map_err(|error| error.to_string())?;
                    if self.ui.profile_audio
                        && matches!(apply, crate::domain::state::ApplyAdmission::Open { .. })
                    {
                        engine
                            .require_profile_restore_audio(apply.id())
                            .map_err(|error| error.to_string())?;
                    }
                    self.persistence.admit_user_apply(
                        apply,
                        submitted,
                        binding.expect("engine binding"),
                    );
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
                    if let Some(apply) = self.persistence.authorized_apply() {
                        self.persistence.cancel_user_apply(apply);
                    }
                }
                Command::Quit(expected) => {
                    Self::checked_state(engine, expected).map_err(|error| error.to_string())?;
                    if self.persistence.request_close() {
                        engine.quit();
                    }
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
                    self.persistence.set_gain_preference(gain);
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
                    self.persistence.set_gain_preference(gain);
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
            Err(error) => self.qualification_rejection(line, error),
        }
        self.poll()
    }
    pub(crate) fn poll(&mut self) -> UiUpdate {
        if self.ui.catalog.poll() {
            self.dirty = true;
            self.observe_profile_snapshot();
        }
        self.prepare_startup_adaptation();
        self.finish_source_selection();
        self.pump_output();
        if let Some(engine) = &mut self.engine {
            if let Some((key, _)) = engine.model().opening() {
                self.persistence.observe_opening(key);
            }
            engine.poll();
            if let Some(apply) = self.persistence.authorized_apply()
                && !engine.has_user_apply_result_or_pending(apply)
            {
                self.persistence.cancel_user_apply(apply);
            }
            if let Some((key, _)) = engine.model().opening() {
                self.persistence.observe_opening(key);
            }
            if let Some(event) = engine.take_verified_applied() {
                let apply = match &event {
                    VerifiedApplied::Open { key, .. } => key.apply,
                    VerifiedApplied::Treatments { key, .. } => key.apply,
                };
                if let Some((_, source)) = self
                    .ui
                    .submitted
                    .take_if(|(submitted, _)| *submitted == apply)
                    && self.ui.selected == source
                {
                    self.ui.source_explicit = false;
                }
                if matches!(&event, VerifiedApplied::Open { key, .. } if self.startup_apply == Some(key.apply))
                {
                    self.startup_apply = None;
                    self.persistence.verified_applied(event);
                    self.persistence.startup_verified();
                } else {
                    self.persistence.verified_applied(event);
                }
                self.dirty = true;
            }
            if self.startup_apply.is_some()
                && engine.model().opening().is_none()
                && engine.model().validation_request().is_none()
                && engine.model().cleanup() == &CleanupStatus::Complete
                && !matches!(engine.model().phase(), ProductPhase::Active)
            {
                self.startup_apply = None;
                let reason = engine
                    .model()
                    .validation_rejection()
                    .map(|rejection| rejection.failure.to_string())
                    .or_else(|| {
                        engine
                            .model()
                            .failures()
                            .map(|failures| format!("{failures:?}"))
                    })
                    .unwrap_or_else(|| "source disappeared during initial restoration".into());
                self.persistence.startup_failed(reason);
                self.dirty = true;
            }
        }
        self.sync_ui_tokens();
        let mut update = self.poll_media();
        let base_drained = self.quit_empty
            || self
                .engine
                .as_ref()
                .is_some_and(|engine| engine.model().shutdown_ready());
        let output_drained = base_drained && self.drain_catalog();
        let capture_drained = if !base_drained {
            false
        } else if let Some(worker) = &mut self.ui.catalog.worker {
            worker.stop();
            match worker.try_join() {
                Some(result) => {
                    if let Err(error) = result {
                        self.ui.catalog.worker_error = error;
                        self.dirty = true;
                    }
                    self.ui.catalog.worker = None;
                    true
                }
                None => false,
            }
        } else {
            true
        };
        let drained = output_drained && capture_drained;
        let previous_dialog = self.persistence.close_dialog();
        if drained {
            self.persistence.drained();
        }
        if previous_dialog != self.persistence.close_dialog() {
            update.changed = true;
        }
        update.quit = drained && self.persistence.quit_allowed();
        if update.quit {
            // The host ignores unchanged updates; a quit authorization that
            // changes no other published field must still be applied or the
            // GUI never leaves the event loop.
            update.changed = true;
        }
        if update.changed {
            let projection = self.output_projection();
            let snapshot = self.product_snapshot(&update, &projection);
            if self.ui.snapshot.as_ref() != Some(&snapshot) {
                update.ui_snapshot =
                    serde_json::to_string(&snapshot).expect("finite validated typed UI snapshot");
                self.ui.snapshot = Some(snapshot);
            }
            self.last_output = Some(projection.clone());
            update.output_rows = projection.rows;
            update.output_catalog_revision = projection.catalog_revision;
            update.output_selected = projection.selected;
            update.output_selected_key = projection.selected_key;
            update.output_effective = projection.effective;
            update.output_status = projection.status;
            update.output_needs_action = projection.needs_action;
            update.settings_status = self.persistence.status();
            update.settings_path = self.persistence.path();
            update.settings_refused = self.persistence.refused();
            update.startup_reason = self.persistence.startup_reason().into();
            update.saved_selection = self
                .engine
                .as_ref()
                .map(|engine| selection_text(&engine.model().draft().settings))
                .unwrap_or_default();
            update.close_dialog = self.persistence.close_dialog().into();
            update.reset_token = self.persistence.reset_token();
            update.fullscreen = self.persistence.preferences().fullscreen;
            update.stats_hidden = self.persistence.preferences().stats_hidden;
            update.closing = !self.persistence.mutations_open();
        }
        if update.changed {
            tracing::info!(settings_status = %update.settings_status, settings_path = %update.settings_path,
                settings_refused = update.settings_refused, startup_reason = %update.startup_reason,
                close_dialog = %update.close_dialog, reset_token = update.reset_token,
                fullscreen = update.fullscreen, selection = %update.saved_selection, quit = update.quit, "settings_runtime");
        }
        update
    }
    fn poll_media(&mut self) -> UiUpdate {
        let Some(engine) = &mut self.engine else {
            let changed = std::mem::take(&mut self.dirty);
            return UiUpdate {
                ui_snapshot: String::new(),
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
                        "No capture selected. Choose a capture source.".into()
                    } else {
                        self.command_error.clone()
                    }
                } else {
                    String::new()
                },
                context_diagnostic: if changed {
                    if self.command_error.is_empty() {
                        "No capture selected. Choose a capture source.".into()
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
                volume_percent: i32::from(self.persistence.preferences().gain.volume_percent),
                muted: self.persistence.preferences().gain.muted,
                can_toggle_pause: false,
                can_set_gain: self.persistence.mutations_open(),
                playback_status: if changed {
                    "Unavailable".into()
                } else {
                    String::new()
                },
                create_native: false,
                release_native: false,
                quit: self.quit_empty,
                settings_status: String::new(),
                settings_path: String::new(),
                settings_refused: false,
                saved_selection: String::new(),
                startup_reason: String::new(),
                close_dialog: String::new(),
                reset_token: 0,
                fullscreen: false,
                stats_hidden: false,
                closing: false,
                output_rows: String::new(),
                output_catalog_revision: 0,
                output_selected: String::new(),
                output_selected_key: String::new(),
                output_effective: String::new(),
                output_status: String::new(),
                output_needs_action: false,
            };
        };
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
        let desired_settings = stream_settings(model);
        let desired_source = desired_settings
            .audio
            .source()
            .filter(|_| changed)
            .map(|source| source.name().to_owned())
            .unwrap_or_default();
        let desired_audio_enabled = desired_settings.audio.enabled();
        let can_open = self.persistence.mutations_open()
            && self.ui.resolving.is_none()
            && (model.can_apply() || model.can_reconnect());
        let can_restart =
            self.persistence.mutations_open() && (model.can_restart() || model.can_reconnect());
        let restart_generation = identity.attempt().map(AttemptId::get).unwrap_or(0);
        let quit = model.shutdown_ready();
        let mut diagnostic = String::new();
        if changed {
            if let Some(rejection) = model.validation_rejection() {
                diagnostic.push_str(&rejection.failure.to_string());
                if let Some(treatment) = &rejection.failure.treatment {
                    diagnostic.push_str(&format!(
                        "\nTreatment diagnostics: {}",
                        serde_json::json!(treatment)
                    ));
                }
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
                    if let Some(treatment) = &failure.treatment {
                        diagnostic.push_str(&format!(
                            "\nTreatment diagnostics: {}",
                            serde_json::json!(treatment)
                        ));
                    }
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
            ProductPhase::ApplyingTreatments => "Applying treatments",
            ProductPhase::RestoringTreatments => "Restoring treatments",
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
        let can_set_gain = self.persistence.mutations_open() && engine.gain_admission_open();
        let volume_percent = i32::from(self.persistence.preferences().gain.volume_percent);
        let muted = self.persistence.preferences().gain.muted;
        // Actual audio availability is canonical coordinator state, never the
        // desired draft: a draft edit or a desired request cannot masquerade
        // as a running route.
        let availability = engine.audio_availability().filter(|_| changed).cloned();
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
                        AudioSilence::Output(reason) => output_silence_text(reason),
                        AudioSilence::Failed(error) => format!("audio route failed: {error}"),
                    };
                    ("Silent".to_owned(), String::new(), reason)
                }
                Some(AudioAvailability::Opening { epoch }) => (
                    "Opening".to_owned(),
                    String::new(),
                    format!("opening audio route (epoch {})", epoch.get()),
                ),
                Some(AudioAvailability::Switching { epoch, revision }) => (
                    "Switching".to_owned(),
                    String::new(),
                    format!(
                        "switching output route (epoch {}, output revision {})",
                        epoch.get(),
                        revision.get()
                    ),
                ),
                Some(AudioAvailability::Active { route }) => (
                    "Active".to_owned(),
                    route.source().name().to_owned(),
                    String::new(),
                ),
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
        let treatment_failed = model.failures().is_some_and(|failures| {
            failures
                .candidate
                .iter()
                .chain(failures.restore.iter())
                .chain(failures.incumbent.iter())
                .any(|failure| failure.treatment.is_some())
        }) || model
            .validation_rejection()
            .is_some_and(|rejection| rejection.failure.treatment.is_some());
        if changed
            && diagnostic.is_empty()
            && let Some(fatal) = engine.runner_mut().fatal_native_failure()
        {
            diagnostic = fatal.to_string();
        }
        let mut context_diagnostic = diagnostic.clone();
        if changed && engine.model().active().is_some() {
            let stats = selection_text(stream_settings(engine.model()));
            diagnostic = if diagnostic.is_empty() {
                format!("Active selection:\n{stats}\n")
            } else {
                format!("Active selection:\n{stats}\n{diagnostic}")
            };
        }
        if changed && diagnostic.is_empty() {
            diagnostic = if engine.runner_mut().report.is_empty() {
                format!(
                    "Requested: {:?}\nAudio requested: {desired_audio_enabled}\nOpen capture to start the selected configuration.",
                    engine.model().draft().settings.video
                )
            } else {
                engine.runner_mut().report.clone()
            };
        }
        if changed && context_diagnostic.is_empty() && engine.model().active().is_none() {
            context_diagnostic = "Open capture to start the selected configuration.".into();
        }
        if changed && renderer_interpolation_inactive(engine.model()) {
            if !diagnostic.is_empty() {
                diagnostic.push('\n');
            }
            diagnostic.push_str(INTERPOLATION_INACTIVE_TEXT);
            if !context_diagnostic.is_empty() {
                context_diagnostic.push('\n');
            }
            context_diagnostic.push_str(INTERPOLATION_INACTIVE_TEXT);
        }
        if changed {
            log_snapshot(engine);
        }
        let runner = engine.runner_mut();
        UiUpdate {
            ui_snapshot: String::new(),
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
            failed: phase == GatePhase::Failed
                || runner.fatal_native_failure().is_some()
                || treatment_failed,
            diagnostic,
            context_diagnostic,
            recovery_evidence,
            recovery_stage,
            candidates,
            paused,
            prepared_paused,
            volume_percent,
            muted,
            can_toggle_pause,
            can_set_gain,
            playback_status: if changed {
                playback_status.into()
            } else {
                String::new()
            },
            create_native: native.create_native,
            release_native: native.release_native,
            quit,
            settings_status: String::new(),
            settings_path: String::new(),
            settings_refused: false,
            saved_selection: String::new(),
            startup_reason: String::new(),
            close_dialog: String::new(),
            reset_token: 0,
            fullscreen: false,
            stats_hidden: false,
            closing: false,
            output_rows: String::new(),
            output_catalog_revision: 0,
            output_selected: String::new(),
            output_selected_key: String::new(),
            output_effective: String::new(),
            output_status: String::new(),
            output_needs_action: false,
        }
    }
    pub(crate) fn unchanged(&mut self) -> UiUpdate {
        self.poll()
    }
}

fn stream_settings(model: &ProductModel) -> &DraftSettings {
    model
        .active()
        .map(|active| active.applied().settings())
        .or_else(|| model.recovery().map(|recovery| recovery.applied.settings()))
        .or_else(|| model.last_valid().map(|applied| applied.settings()))
        .unwrap_or(&model.draft().settings)
}

fn selection_text(settings: &DraftSettings) -> String {
    let identity = &settings.video.identity;
    let mode = settings.video.mode;
    let ports = identity
        .topology()
        .ports()
        .iter()
        .map(|port| port.get().to_string())
        .collect::<Vec<_>>()
        .join(".");
    let source = settings
        .audio
        .source()
        .map(|source| format!("{source:?}"))
        .unwrap_or_else(|| "none".into());
    let policy = match mode.format_policy {
        crate::domain::capture::InputFormatPolicy::Auto => "Auto".to_owned(),
        crate::domain::capture::InputFormatPolicy::Manual { fourcc } => {
            format!("Manual {}", String::from_utf8_lossy(&fourcc.bytes()))
        }
    };
    let resolved = settings
        .video
        .resolved_fourcc
        .map(|fourcc| {
            format!(
                "{} ({:#010x})",
                String::from_utf8_lossy(&fourcc.bytes()),
                fourcc.kernel_value()
            )
        })
        .unwrap_or_else(|| "not prepared".into());
    format!(
        "USB {:04x}:{:04x}\nController {} / ports {} / serial {}\nInput policy: {} · {}×{} · {}/{} FPS\nResolved input: {} (advertised selection, not negotiated proof)\nAudio {}: {}",
        identity.vendor_id(),
        identity.product_id(),
        identity.topology().controller(),
        ports,
        identity.serial().unwrap_or("none"),
        policy,
        mode.size.width(),
        mode.size.height(),
        mode.rate.numerator(),
        mode.rate.denominator(),
        resolved,
        if settings.audio.enabled() {
            "enabled"
        } else {
            "disabled (retained)"
        },
        source
    )
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

/// Whether interpolation is requested by the stored draft or already applied
/// by the active session. Either way the setting is inactive under the current
/// low-latency timing and the presentation carries the exact limitation note.
fn renderer_interpolation_inactive(model: &ProductModel) -> bool {
    model.draft().settings.renderer.values().interpolation
        || model
            .active()
            .is_some_and(|active| active.applied().settings().renderer.values().interpolation)
}

/// Human-readable product phase: the reducer-level truth, projected next to
/// the media gate phase so disconnected/recovering/selection states stay
/// visible even while no media window exists.
fn phase_name(phase: ProductPhase) -> &'static str {
    match phase {
        ProductPhase::Stopped => "Stopped",
        ProductPhase::Active => "Active",
        ProductPhase::ApplyingTreatments => "ApplyingTreatments",
        ProductPhase::RestoringTreatments => "RestoringTreatments",
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

fn runtime_snapshot(engine: &mut Engine) -> serde_json::Value {
    let pending_key = engine.model().treating().map(|transition| transition.key());
    // Admission can precede the owner's first Pending publication. A retained
    // incumbent confirmation is not success for that newly admitted key.
    let treatments = match (pending_key, engine.runner_mut().treatment_snapshot()) {
        (Some(key), observed)
            if observed.is_none_or(|observed| {
                observed.key != key
                    || matches!(
                        observed.status,
                        crate::media::controller::TreatmentStatus::Confirmed(_)
                    )
            }) =>
        {
            Some(serde_json::json!({
                "key": key, "sequence": null, "status": "Pending",
                "confirmation": null, "failure": null,
            }))
        }
        (_, Some(treatments)) => {
            let (status, confirmation, failure) = match &treatments.status {
                crate::media::controller::TreatmentStatus::Pending => ("Pending", None, None),
                crate::media::controller::TreatmentStatus::Confirmed(confirmation) => {
                    ("Confirmed", Some(confirmation), None)
                }
                crate::media::controller::TreatmentStatus::Failed(failure) => {
                    ("Failed", None, Some(failure))
                }
            };
            Some(serde_json::json!({
                "key": treatments.key, "sequence": treatments.sequence, "status": status,
                "confirmation": confirmation, "failure": failure,
            }))
        }
        _ => None,
    };
    let model = engine.model();
    let identity = model.state_identity();
    serde_json::json!({
        "phase": model.phase(), "apply_id": identity.operation().map(|id| id.get()), "attempt_id": identity.attempt().map(AttemptId::get),
        "treatment_pass": identity.treatment_pass(), "treatments": treatments,
        "cleanup": match model.cleanup() { CleanupStatus::Complete => "Complete", CleanupStatus::Draining => "Draining", CleanupStatus::Blocked { .. } => "Blocked" },
        "draft_revision": model.draft().revision.get(), "draft": model.draft().settings,
        "last_valid": model.last_valid().map(|settings| settings.settings()),
        "active_attempt": model.active().map(|active| active.attempt().get()), "active_settings": model.active().map(|active| active.applied().settings()),
        "active_playback": model.active().map(|active| active.playback()),
        "failures": model.failures(), "validation_rejection": model.validation_rejection().map(|rejection| serde_json::json!({"request": rejection.request, "failure": rejection.failure})),
        "gain": engine.gain(), "can_apply": model.can_apply(), "can_restart": model.can_restart(), "can_reconnect": model.can_reconnect(), "shutdown_ready": model.shutdown_ready(),
    })
}

fn log_snapshot(engine: &mut Engine) {
    tracing::info!(snapshot = %runtime_snapshot(engine), "apply_runtime");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capture::{PreparedCapture, apply::fixture_prepared, linux::session_fixture},
        domain::{
            capture::{AudioEpoch, AudioError, CaptureMode, CapturedFourCc, FrameRate, FrameSize},
            failure::ApplyFailure,
            state::{ProductPhase, ValidationRequest},
        },
        media::controller::{
            BackendEvent, Generation, OwnerEndpoint, X11WindowId,
            test_support::{Config, Driver},
        },
    };
    use std::sync::mpsc;

    /// Legitimate cfg(test) fixture bundle: real captured filter inventory plus
    /// the real captured renderer inventory (owner keeps the production query).
    fn fixture_treatment_capabilities()
    -> crate::media::render_catalog::QualifiedTreatmentCapabilities {
        crate::media::render_catalog::QualifiedTreatmentCapabilities {
            filters: Ok(crate::media::filter_catalog::fixture_capabilities()),
            renderer: Ok(crate::media::render_catalog::fixture_capabilities()),
        }
    }
    fn settings() -> DraftSettings {
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(1280, 720).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        DraftSettings {
            filters: crate::domain::filters::FilterChain::default(),
            video: crate::domain::capture::VideoRequest::manual(
                crate::domain::capture::ModeRequest {
                    identity: session_fixture(&["/dev/video0"], mode).devices()[0]
                        .identity()
                        .clone(),
                    mode,
                },
            ),
            audio: AudioSelection::default(),
            renderer: crate::domain::render::RendererSettings::default(),
        }
    }
    fn validate(
        request: ValidationRequest,
    ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>) {
        let prepared = fixture_prepared(request.settings.clone());
        (request, prepared)
    }
    fn validate_product(
        request: ValidationRequest,
    ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>) {
        let prepared = crate::capture::apply::fixture_request_prepared(
            &request,
            &session_fixture(&["/dev/video0"], settings().video.fixture_mode()),
            request.watch,
        );
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
        runtime_fixture(execute, None, None)
    }
    fn runtime_fixture(
        execute: fn(
            ValidationRequest,
        ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>),
        fail_spawn: Option<u64>,
        snapshot: Option<crate::capture::linux::CaptureSnapshot>,
    ) -> (RuntimeCoordinator, mpsc::Receiver<Driver>) {
        let (tx, rx) = mpsc::channel();
        let runner = GateRunner::with_spawner(
            fixture_treatment_capabilities(),
            move |generation, config| {
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
                let backend = backend.with_open_treatments(config.treatment_key, config.treatments);
                tx.send(driver).unwrap();
                OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || backend)
            },
        );
        let engine = ApplyCoordinator::new(
            settings(),
            PlaybackGain::default(),
            match snapshot {
                Some(snapshot) => {
                    CaptureValidator::with_runner_and_snapshot(execute, snapshot, Vec::new())
                }
                None => CaptureValidator::with_runner(execute),
            },
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
                persistence: {
                    let mut session = PersistenceSession::load(Err("test has no store".into()));
                    session.prepare_startup(&StartupSelection {
                        restore_profile: false,
                        source_binding: None,
                        topology_pin: None,
                        adapt_video: false,
                        draft: Some(settings()),
                        auto_open: false,
                        reason: String::new(),
                        audio_policy: crate::app::settings::StartupAudioPolicy::Explicit,
                        bootstrap_audio: Default::default(),
                    });
                    session
                },
                auto_open: false,
                startup_apply: None,
                output: OutputPolicy::new(PersistentOutputChoice::default()),
                catalog: None,
                // Tests never touch a real Pulse connection.
                catalog_start_failed: true,
                registered_target: None,
                fed_plan_revision: None,
                last_catalog: None,
                last_rows: Vec::new(),
                last_output: None,
                catalog_stopping: false,
                catalog_join_error: None,
                ui: ProductUi::default(),
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
    /// The post-open committed watch is subscribed during the opening poll while
    /// its authoritative observation is published by the fixture observation
    /// worker and drained by a later poll. Settle that observation before
    /// comparing exact `StateIdentity` values, which carry the stamp; later
    /// polls are stable until a new subscription or validation refresh.
    #[track_caller]
    fn settle_committed_watch(runtime: &mut RuntimeCoordinator) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            runtime.poll();
            let model = runtime.engine.as_ref().unwrap().model();
            if model.observation().is_some_and(|observation| {
                model.watch_target().unwrap().watch == observation.stamp.watch
            }) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "committed watch observation was not delivered"
            );
            std::thread::yield_now();
        }
    }
    #[test]
    fn stream_stats_audio_uses_description_then_retained_name_without_debug_properties() {
        let (mut runtime, _) = runtime();
        let identity = runtime.sources[0].clone();
        let mut selected = settings();
        selected.audio = AudioSelection::Disabled {
            retained: Some(identity.clone()),
        };
        runtime.ui.catalog.audio = vec![crate::capture::audio::AudioSource {
            identity: identity.clone(),
            description: "Capture input".into(),
        }];
        let stats = runtime.stream_stats(&selected);
        assert!(!stats.audio_enabled);
        assert_eq!(
            stats.audio_input_label,
            runtime.ui.catalog.audio[0].description
        );
        runtime.ui.catalog.audio[0].description.clear();
        assert_eq!(
            runtime.stream_stats(&selected).audio_input_label,
            identity.name()
        );
        runtime.ui.catalog.audio.clear();
        assert_eq!(
            runtime.stream_stats(&selected).audio_input_label,
            identity.name()
        );
        selected.audio = AudioSelection::default();
        assert!(runtime.stream_stats(&selected).audio_input_label.is_empty());
        cleanup(&mut runtime);
    }

    #[test]
    fn stream_stats_keep_incumbent_and_recovery_frozen_settings_instead_of_candidate() {
        let (mut runtime, drivers) = runtime();
        let driver = start_live(&mut runtime, &drivers);
        let incumbent = runtime
            .ui
            .snapshot
            .as_ref()
            .unwrap()
            .stream_stats
            .clone()
            .unwrap();
        let revision = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .revision
            .get();
        runtime.qualification_command(&format!("draft-video {revision} YUYV 1920x1080 30/1"));
        let snapshot = runtime.ui.snapshot.as_ref().unwrap();
        assert_eq!(snapshot.stream_stats.as_ref(), Some(&incumbent));
        assert_ne!(
            snapshot.selection.as_ref().unwrap().size,
            Some(incumbent.size)
        );
        driver.send(BackendEvent::EndFile {
            reason: 0,
            error: 0,
        });
        driver.fence();
        let lost = await_update(&mut runtime, |update| {
            update.product_phase == "Disconnected"
        });
        assert!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .recovery()
                .is_some()
        );
        assert_eq!(
            runtime.ui.snapshot.as_ref().unwrap().stream_stats.as_ref(),
            Some(&incumbent)
        );
        if lost.release_native {
            runtime.native_released(AttemptId::new(lost.generation).unwrap());
        }
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn hidden_stats_preserve_command_and_interpolation_diagnostics() {
        let (mut runtime, _) = runtime();
        let rejected = runtime.qualification_command("invalid command");
        let hidden = runtime.set_stats_hidden(true);
        assert!(hidden.stats_hidden);
        assert_eq!(hidden.context_diagnostic, rejected.context_diagnostic);
        assert_eq!(hidden.diagnostic, rejected.diagnostic);
        let revision = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .revision
            .get();
        let update = runtime.qualification_command(&format!(
            "draft-renderer {revision} {}",
            serde_json::to_string(&renderer_settings_with_interpolation(true)).unwrap()
        ));
        assert!(update.stats_hidden);
        assert!(
            update
                .context_diagnostic
                .contains(INTERPOLATION_INACTIVE_TEXT)
        );
        cleanup(&mut runtime);
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
        driver.confirm_open_treatments();
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
            ExpectedState {
                treatment_pass: Some(
                    if expected.treatment_pass
                        == Some(crate::domain::state::TreatmentPass::Candidate)
                    {
                        crate::domain::state::TreatmentPass::Restore
                    } else {
                        crate::domain::state::TreatmentPass::Candidate
                    },
                ),
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

    struct WritableSettings(std::path::PathBuf);
    impl WritableSettings {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "furami-runtime-settings-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir(&directory).unwrap();
            Self(directory)
        }
        fn path(&self) -> std::path::PathBuf {
            self.0.join("settings.json")
        }
        fn applied(&self) -> DraftSettings {
            let (_, crate::settings::LoadOutcome::Loaded(document)) =
                crate::settings::SettingsStore::load(self.path())
            else {
                panic!("runtime did not publish a valid settings document")
            };
            let identity = document.last_source.as_ref().expect("verified last source");
            document
                .profiles
                .entries()
                .iter()
                .find(|profile| &profile.settings().video.identity == identity)
                .expect("confirmed user Apply must be saved")
                .settings()
                .clone()
        }
        fn bind(&self, runtime: &mut RuntimeCoordinator) {
            runtime.persistence = PersistenceSession::load(Ok(self.path()));
            runtime.persistence.prepare_startup(&StartupSelection {
                restore_profile: false,
                source_binding: None,
                topology_pin: None,
                adapt_video: false,
                draft: Some(
                    runtime
                        .engine
                        .as_ref()
                        .unwrap()
                        .model()
                        .draft()
                        .settings
                        .clone(),
                ),
                auto_open: false,
                reason: String::new(),
                audio_policy: crate::app::settings::StartupAudioPolicy::Explicit,
                bootstrap_audio: Default::default(),
            });
        }
    }
    impl Drop for WritableSettings {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn treatment_chain(all_disabled: bool) -> crate::domain::filters::FilterChain {
        use crate::domain::filters::*;
        FilterChain::new(vec![
            FilterEntry::new(
                "SDR\u{a0}reference".into(),
                Filter::Format(FormatParams::new(
                    SdrMatrix::Bt709,
                    ColorLevels::Limited,
                    SdrGamma::Bt1886,
                )),
                !all_disabled,
            ),
            FilterEntry::new(
                "disabled\n\0equalizer".into(),
                Filter::Eq(
                    EqParams::new(EqValues {
                        contrast: 1.1,
                        brightness: 0.05,
                        saturation: 1.2,
                        gamma: 1.0,
                        gamma_r: 1.0,
                        gamma_g: 1.0,
                        gamma_b: 1.0,
                        gamma_weight: 1.0,
                    })
                    .unwrap(),
                ),
                false,
            ),
            FilterEntry::new(
                "denoise".into(),
                Filter::Hqdn3d(
                    Hqdn3dParams::new(Hqdn3dValues {
                        luma_spatial: 3.0,
                        chroma_spatial: 0.0,
                        luma_tmp: 6.0,
                        chroma_tmp: 4.5,
                    })
                    .unwrap(),
                ),
                !all_disabled,
            ),
        ])
        .unwrap()
    }
    fn edit_filters(
        runtime: &mut RuntimeCoordinator,
        chain: &crate::domain::filters::FilterChain,
    ) -> UiUpdate {
        let revision = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .revision
            .get();
        runtime.qualification_command(&format!(
            "draft-filters {revision} {}",
            serde_json::to_string(chain).unwrap(),
        ))
    }
    fn begin_live_treatments(
        runtime: &mut RuntimeCoordinator,
        driver: &Driver,
    ) -> (
        crate::media::controller::RequestId,
        crate::domain::state::TreatmentAttemptKey,
    ) {
        let expected = snapshot_expected(runtime);
        let revision = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .revision
            .get();
        let pending = runtime.qualification_command(&format!(
            "apply {:?} {} {} Complete {revision}",
            expected.phase, expected.apply, expected.attempt,
        ));
        assert_eq!(pending.product_phase, "ApplyingTreatments");
        assert_eq!(pending.playback_status, "Applying treatments");
        assert!(
            !pending.failed
                && !pending.can_open
                && !pending.can_restart
                && !pending.can_toggle_pause
        );
        assert!(pending.can_set_gain);
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let crate::media::controller::BackendCommand::ApplyTreatments { key, .. } = command else {
            panic!("expected exactly one whole-chain treatment command: {command:?}")
        };
        assert_eq!(key.pass, crate::domain::state::TreatmentPass::Candidate);
        assert_eq!(runtime.persistence.authorized_apply(), Some(key.apply));
        (id, key)
    }
    /// Open aggregate keyed to the physical opening: the media owner returns it
    /// only after nine native renderer readbacks plus its vf proof.
    fn verify_open(model: &mut ProductModel, key: crate::domain::state::AttemptKey) {
        use crate::domain::state::{TreatmentAttemptKey, TreatmentPass};
        let request = model.opening_request().expect("opening request for proof");
        let treatment = TreatmentAttemptKey {
            apply: key.apply,
            attempt: key.attempt,
            pass: TreatmentPass::Open,
        };
        let confirmation = crate::media::controller::test_support::treatment_confirmation(
            treatment,
            request.playback,
            request.settings.renderer.clone(),
        );
        model.open_verified(key, confirmation);
    }
    /// A live-origin confirmation binding the real submitted key to the
    /// default renderer the draft carries; the vf graph is treated as
    /// reconfigured, which is what a genuinely changed compiled chain proves.
    fn treatment_confirmation(
        key: crate::domain::state::TreatmentAttemptKey,
    ) -> crate::domain::state::TreatmentConfirmation {
        use crate::domain::state::{
            FilterConfirmation, FilterTreatmentProof, RendererConfirmation, TreatmentConfirmation,
        };
        let filter = FilterConfirmation::checked(key, 0.0, 2.0, 32).unwrap();
        let renderer =
            RendererConfirmation::checked(key, crate::domain::render::RendererSettings::default());
        TreatmentConfirmation::checked(
            key,
            crate::domain::state::InitialPlayback::Live,
            true,
            FilterTreatmentProof::Reconfigured(filter),
            renderer,
        )
        .unwrap()
    }
    fn confirm_treatments(
        driver: &Driver,
        id: crate::media::controller::RequestId,
        key: crate::domain::state::TreatmentAttemptKey,
    ) {
        driver.send(BackendEvent::TreatmentResult {
            id: id.get(),
            key,
            result: Ok(treatment_confirmation(key)),
        });
        driver.fence();
    }
    /// Confirms a candidate whose filter chain equals the incumbent's, exactly
    /// as a real backend would: the vf graph is preserved on its committed
    /// origin and the renderer proof carries the actually submitted renderer.
    fn confirm_unchanged_filter_treatments(
        runtime: &RuntimeCoordinator,
        driver: &Driver,
        id: crate::media::controller::RequestId,
        key: crate::domain::state::TreatmentAttemptKey,
        submitted_renderer: crate::domain::render::RendererSettings,
    ) {
        use crate::domain::state::{
            FilterConfirmation, FilterEvidence, FilterTreatmentProof, RendererConfirmation,
            TreatmentConfirmation,
        };
        let origin = runtime
            .engine
            .as_ref()
            .expect("confirmed vf origin")
            .model()
            .confirmed_filter_origin()
            .expect("an incumbent committed vf origin");
        let confirmation = TreatmentConfirmation::checked(
            key,
            crate::domain::state::InitialPlayback::Live,
            false,
            FilterTreatmentProof::Preserved {
                origin: FilterEvidence::Confirmed(
                    FilterConfirmation::checked(origin, 0.0, 2.0, 32).unwrap(),
                ),
            },
            RendererConfirmation::checked(key, submitted_renderer),
        )
        .expect("unchained-filter proof");
        driver.send(BackendEvent::TreatmentResult {
            id: id.get(),
            key,
            result: Ok(confirmation),
        });
        driver.fence();
    }
    fn treatment_failure(
        key: crate::domain::state::TreatmentAttemptKey,
        kind: crate::domain::failure::FilterErrorKind,
        fresh: bool,
    ) -> Box<crate::domain::failure::TreatmentFailure> {
        use crate::domain::failure::{
            FilterAttemptDiagnostics, FilterFailure, TreatmentErrorKind, TreatmentFailure,
            TreatmentFailureDisposition,
        };
        Box::new(TreatmentFailure {
            key: Some(key),
            kind: TreatmentErrorKind::Filter(FilterFailure {
                kind,
                attributed_ordinal: None,
                requires_fresh_owner: fresh,
                diagnostics: FilterAttemptDiagnostics {
                    key: Some(key),
                    entries: Vec::new(),
                    records: Vec::new(),
                    native_evidence_lost: false,
                    truncated: false,
                    dropped_context: 0,
                },
            }),
            disposition: if fresh {
                TreatmentFailureDisposition::RestoreFreshOwner
            } else {
                TreatmentFailureDisposition::RestoreSameOwner
            },
        })
    }

    #[test]
    fn qualifier_strict_filter_edit_preserves_invalid_draft_and_legacy_boundaries() {
        let (mut runtime, drivers) = runtime();
        let chain = treatment_chain(false);
        let edited = edit_filters(&mut runtime, &chain);
        assert!(!edited.failed);
        let frozen = runtime.engine.as_ref().unwrap().model().draft().clone();
        assert_eq!(frozen.settings.filters, chain);
        for invalid in [
            r#"draft-filters 1 {"entries":null}"#,
            r#"draft-filters 1 {"entries":[],"entries":[]}"#,
            "draft-filters 0 {\"entries\":[]}",
            "draft-filters 1 {\"entries\":[{\"label\":\"raw\ncontrol\"}]}",
        ] {
            let update = runtime.qualification_command(invalid);
            assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &frozen);
            assert!(update.changed && runtime.command_error.starts_with('{'));
            assert!(!runtime.command_error.contains('\n') && !runtime.command_error.contains('\0'));
            let diagnostic: serde_json::Value =
                serde_json::from_str(&runtime.command_error).unwrap();
            assert_eq!(diagnostic["input"], invalid);
            assert!(diagnostic["error"].is_string());
        }
        let mut exact = format!("draft-filters 1 {}", serde_json::to_string(&chain).unwrap());
        exact.push_str(&" ".repeat(control::STRUCTURED_COMMAND_MAX_BYTES - exact.len()));
        runtime.qualification_command(&exact);
        assert!(runtime.command_error.is_empty());
        let accepted = runtime.engine.as_ref().unwrap().model().draft().clone();
        let oversize = format!("{exact} ");
        runtime.qualification_command(&oversize);
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &accepted);
        for legacy in [
            format!("settings{}", " ".repeat(256)),
            "settings\u{a0}".into(),
            "preference-mute true\0".into(),
        ] {
            runtime.qualification_command(&legacy);
            assert!(!runtime.command_error.is_empty());
            assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &accepted);
        }
        assert!(drivers.try_recv().is_err());
        cleanup(&mut runtime);
    }

    #[test]
    fn runtime_confirmed_open_and_live_filter_receipts_store_full_chains_not_newer_drafts() {
        for all_disabled in [false, true] {
            let store = WritableSettings::new();
            let (mut runtime, drivers) = runtime();
            store.bind(&mut runtime);
            let chain = treatment_chain(all_disabled);
            edit_filters(&mut runtime, &chain);
            let driver = start_live(&mut runtime, &drivers);
            assert_eq!(store.applied().filters, chain);
            assert_eq!(runtime.persistence.authorized_apply(), None);
            let initial = runtime_snapshot(runtime.engine.as_mut().unwrap());
            assert_eq!(initial["treatments"]["status"], "Confirmed");
            assert_eq!(initial["treatments"]["key"]["pass"], "Open");
            assert_eq!(
                initial["active_settings"]["filters"],
                serde_json::json!(&chain)
            );
            assert!(initial["treatment_pass"].is_null());
            let candidate = treatment_chain(!all_disabled);
            edit_filters(&mut runtime, &candidate);
            let (id, key) = begin_live_treatments(&mut runtime, &driver);
            let pending = runtime_snapshot(runtime.engine.as_mut().unwrap());
            assert_eq!(pending["phase"], "ApplyingTreatments");
            assert_eq!(pending["treatment_pass"], "Candidate");
            assert_eq!(pending["treatments"]["status"], "Pending");
            assert_eq!(pending["treatments"]["key"], serde_json::json!(key));
            assert_eq!(
                pending["active_settings"]["filters"],
                serde_json::json!(&chain)
            );
            edit_filters(
                &mut runtime,
                &crate::domain::filters::FilterChain::default(),
            );
            confirm_treatments(&driver, id, key);
            let confirmed = await_update(&mut runtime, |update| update.product_phase == "Active");
            assert!(!confirmed.failed);
            assert_eq!(confirmed.playback_status, "Live");
            assert_eq!(store.applied().filters, candidate);
            assert!(
                runtime
                    .engine
                    .as_ref()
                    .unwrap()
                    .model()
                    .draft()
                    .settings
                    .filters
                    .entries()
                    .is_empty()
            );
            let snapshot = runtime_snapshot(runtime.engine.as_mut().unwrap());
            assert_eq!(snapshot["treatments"]["status"], "Confirmed");
            assert_eq!(snapshot["treatments"]["key"]["pass"], "Candidate");
            assert_eq!(
                snapshot["active_settings"]["filters"],
                serde_json::json!(&candidate)
            );
            let bytes = std::fs::read(store.path()).unwrap();
            // Duplicate native outcomes do not regain consumed persistence permission.
            confirm_treatments(&driver, id, key);
            runtime.poll();
            assert_eq!(std::fs::read(store.path()).unwrap(), bytes);
            assert_eq!(runtime.persistence.authorized_apply(), None);
            assert!(
                drivers.try_recv().is_err(),
                "treatment Apply never reopens the owner"
            );
            cleanup(&mut runtime);
            driver.destroyed.recv().unwrap();
        }
    }

    #[test]
    fn runtime_live_receipt_cannot_borrow_wrong_admission_key_revision_pass_or_submission() {
        use crate::domain::state::{ApplyAdmission, ApplyId, TreatmentPass};
        for mismatch in 0..6 {
            let store = WritableSettings::new();
            let (mut runtime, drivers) = runtime();
            store.bind(&mut runtime);
            let driver = start_live(&mut runtime, &drivers);
            let original = std::fs::read(store.path()).unwrap();
            edit_filters(&mut runtime, &treatment_chain(false));
            let (id, key) = begin_live_treatments(&mut runtime, &driver);
            let mut submitted = runtime.engine.as_ref().unwrap().model().draft().clone();
            let mut wrong = key;
            let mut revision = submitted.revision;
            match mismatch {
                0 => wrong.apply = ApplyId::new(key.apply.get() + 1).unwrap(),
                1 => wrong.attempt = AttemptId::new(key.attempt.get() + 1).unwrap(),
                2 => wrong.pass = TreatmentPass::Restore,
                3 => revision = DraftRevision::new(revision.get() + 1),
                4 => submitted.revision = DraftRevision::new(revision.get() + 1),
                5 => submitted.settings.filters = crate::domain::filters::FilterChain::default(),
                _ => unreachable!(),
            }
            runtime.persistence.admit_user_apply(
                ApplyAdmission::Treatments {
                    key: wrong,
                    revision,
                },
                submitted,
                ProfileBinding::Serial,
            );
            confirm_treatments(&driver, id, key);
            await_update(&mut runtime, |update| update.product_phase == "Active");
            runtime.poll();
            assert_eq!(
                std::fs::read(store.path()).unwrap(),
                original,
                "mismatch {mismatch}"
            );
            assert_eq!(runtime.persistence.authorized_apply(), None);
            cleanup(&mut runtime);
            driver.destroyed.recv().unwrap();
        }
    }

    #[test]
    fn runtime_close_quit_and_reset_cancel_retained_confirmation_before_consumption() {
        for cancellation in ["close", "quit", "reset"] {
            let store = WritableSettings::new();
            let (mut runtime, drivers) = runtime();
            store.bind(&mut runtime);
            let driver = start_live(&mut runtime, &drivers);
            let original = std::fs::read(store.path()).unwrap();
            edit_filters(&mut runtime, &treatment_chain(false));
            let (id, key) = begin_live_treatments(&mut runtime, &driver);
            confirm_treatments(&driver, id, key);
            // Stop exactly between the real engine's commit and the runtime's
            // receipt consumer; this is not a fabricated persistence receipt.
            runtime.engine.as_mut().unwrap().poll();
            assert!(
                runtime
                    .engine
                    .as_ref()
                    .unwrap()
                    .has_user_apply_result_or_pending(key.apply)
            );
            assert_eq!(std::fs::read(store.path()).unwrap(), original);
            let update = match cancellation {
                "close" => runtime.close(key.attempt.get()),
                "quit" => runtime.quit(),
                "reset" => runtime.request_reset(),
                _ => unreachable!(),
            };
            assert_eq!(runtime.persistence.authorized_apply(), None);
            assert_eq!(std::fs::read(store.path()).unwrap(), original);
            if cancellation == "reset" {
                let token = runtime.persistence.reset_token();
                runtime.decide_reset(token, false);
                assert_eq!(std::fs::read(store.path()).unwrap(), original);
            }
            if update.release_native {
                runtime.native_released(AttemptId::new(update.generation).unwrap());
            }
            cleanup(&mut runtime);
            driver.destroyed.recv().unwrap();
            assert!(drivers.try_recv().is_err());
        }
    }

    #[test]
    fn runtime_late_filter_fault_and_source_loss_revoke_unsaved_confirmed_candidate() {
        use crate::domain::failure::{FilterConfirmationFailure, FilterErrorKind};
        for source_loss in [false, true] {
            let store = WritableSettings::new();
            let (mut runtime, drivers) = runtime();
            store.bind(&mut runtime);
            let driver = start_live(&mut runtime, &drivers);
            let original = std::fs::read(store.path()).unwrap();
            let chain = treatment_chain(false);
            edit_filters(&mut runtime, &chain);
            let (id, key) = begin_live_treatments(&mut runtime, &driver);
            confirm_treatments(&driver, id, key);
            runtime.engine.as_mut().unwrap().poll();
            assert!(
                runtime
                    .engine
                    .as_ref()
                    .unwrap()
                    .has_user_apply_result_or_pending(key.apply)
            );
            if source_loss {
                driver.send(BackendEvent::EndFile {
                    reason: 0,
                    error: 0,
                });
            } else {
                driver.send(BackendEvent::TreatmentFault {
                    key,
                    failure: treatment_failure(
                        key,
                        FilterErrorKind::Unconfirmed {
                            reason: FilterConfirmationFailure::EvidenceLost,
                        },
                        true,
                    ),
                });
            }
            driver.fence();
            let revoked = runtime.poll();
            assert_eq!(runtime.persistence.authorized_apply(), None);
            assert_eq!(std::fs::read(store.path()).unwrap(), original);
            assert_eq!(
                runtime
                    .engine
                    .as_ref()
                    .unwrap()
                    .model()
                    .draft()
                    .settings
                    .filters,
                chain
            );
            if revoked.release_native {
                runtime.quit();
                runtime.native_released(AttemptId::new(revoked.generation).unwrap());
            }
            cleanup(&mut runtime);
            driver.destroyed.recv().unwrap();
            assert!(
                drivers.try_recv().is_err(),
                "cancellation during cleanup cannot start restoration"
            );
        }
    }

    #[test]
    fn runtime_one_live_restore_projects_typed_failure_and_never_saves_candidate_or_restore() {
        use crate::domain::{
            failure::{FilterConfirmationFailure, FilterErrorKind},
            state::TreatmentPass,
        };
        for restore_fails in [false, true] {
            let store = WritableSettings::new();
            let (mut runtime, drivers) = runtime();
            store.bind(&mut runtime);
            let driver = start_live(&mut runtime, &drivers);
            runtime.set_stats_hidden(true);
            let original = std::fs::read(store.path()).unwrap();
            let candidate = treatment_chain(false);
            edit_filters(&mut runtime, &candidate);
            let (id, key) = begin_live_treatments(&mut runtime, &driver);
            let stale_close = format!(
                "close ApplyingTreatments {} {} Complete",
                key.apply.get(),
                key.attempt.get()
            );
            driver.send(BackendEvent::TreatmentResult {
                id: id.get(),
                key,
                result: Err(treatment_failure(
                    key,
                    FilterErrorKind::Unconfirmed {
                        reason: FilterConfirmationFailure::Deadline,
                    },
                    false,
                )),
            });
            driver.fence();
            let restoring = await_update(&mut runtime, |update| {
                update.product_phase == "RestoringTreatments"
            });
            assert!(restoring.failed && !restoring.can_open && !restoring.can_toggle_pause);
            assert_eq!(restoring.playback_status, "Restoring treatments");
            assert!(
                restoring.diagnostic.contains("Unconfirmed")
                    && restoring.diagnostic.contains("Deadline")
            );
            assert!(restoring.stats_hidden);
            let shown = runtime.set_stats_hidden(false);
            assert_eq!(shown.context_diagnostic, restoring.context_diagnostic);
            let hidden = runtime.set_stats_hidden(true);
            assert_eq!(hidden.context_diagnostic, restoring.context_diagnostic);
            assert!(!restoring.context_diagnostic.is_empty());
            assert_eq!(runtime.persistence.authorized_apply(), None);
            let (restore_id, command) = driver
                .submitted
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let crate::media::controller::BackendCommand::ApplyTreatments { key: restore, .. } =
                command
            else {
                panic!("the selected route must send one live restore command")
            };
            assert_eq!(restore.pass, TreatmentPass::Restore);
            let snapshot = runtime_snapshot(runtime.engine.as_mut().unwrap());
            assert_eq!(snapshot["treatment_pass"], "Restore");
            runtime.qualification_command(&stale_close);
            assert!(runtime.command_error.contains("stale"));
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().phase(),
                ProductPhase::RestoringTreatments
            );
            if restore_fails {
                driver.send(BackendEvent::TreatmentResult {
                    id: restore_id.get(),
                    key: restore,
                    result: Err(treatment_failure(
                        restore,
                        FilterErrorKind::RuntimeGraph,
                        true,
                    )),
                });
                driver.fence();
                let retired = await_update(&mut runtime, |update| update.release_native);
                assert!(retired.failed);
                driver.destroyed.recv().unwrap();
                runtime.native_released(AttemptId::new(retired.generation).unwrap());
                let failed = runtime.qualification_command("snapshot");
                assert_eq!(failed.product_phase, "ErrorWithoutActive");
                assert!(failed.failed && failed.can_open && failed.can_restart);
                assert_eq!(failed.playback_status, "Unavailable");
                assert!(
                    failed.diagnostic.contains("RuntimeGraph")
                        && failed.diagnostic.contains("Deadline")
                );
            } else {
                confirm_treatments(&driver, restore_id, restore);
                let restored = await_update(&mut runtime, |update| {
                    update.product_phase == "ErrorWithActiveRestored"
                });
                assert!(restored.failed && restored.can_open && restored.can_toggle_pause);
                assert_eq!(restored.playback_status, "Live");
                assert!(restored.diagnostic.contains("Deadline"));
            }
            assert_eq!(std::fs::read(store.path()).unwrap(), original);
            assert_eq!(
                runtime
                    .engine
                    .as_ref()
                    .unwrap()
                    .model()
                    .draft()
                    .settings
                    .filters,
                candidate
            );
            assert!(
                drivers.try_recv().is_err(),
                "a live restore failure cannot start a second route"
            );
            cleanup(&mut runtime);
            if !restore_fails {
                driver.destroyed.recv().unwrap();
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
    fn qualification_auto_draft_remains_unprepared_and_manual_edit_stays_available() {
        use crate::domain::capture::InputFormatPolicy;
        let (mut runtime, drivers) = runtime();
        runtime.poll();
        let auto = runtime.qualification_command("draft-video 0 auto 1920x1080 60000/1001");
        assert!(auto.saved_selection.contains("Input policy: Auto"));
        assert!(
            auto.saved_selection
                .contains("Resolved input: not prepared")
        );
        let draft = &runtime.engine.as_ref().unwrap().model().draft().settings;
        assert_eq!(draft.video.mode.format_policy, InputFormatPolicy::Auto);
        assert!(draft.video.resolved_fourcc.is_none());
        runtime.qualification_command("draft-video 1 BGR3 1920x1080 60000/1001");
        let draft = &runtime.engine.as_ref().unwrap().model().draft().settings;
        assert_eq!(
            draft.video.mode.format_policy,
            InputFormatPolicy::Manual {
                fourcc: CapturedFourCc::from_bytes(*b"BGR3")
            }
        );
        assert!(draft.video.resolved_fourcc.is_none());
        assert!(
            drivers.try_recv().is_err(),
            "draft edits never create a stream or bypass tuple validation"
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
        let Some(ModelEffect::Open { key: old, .. }) =
            model.fixture_validation_succeeded(&initial_request)
        else {
            panic!("initial physical opening missing");
        };
        assert!(!prepared_paused_presentation(&model, Some(old.attempt)));
        verify_open(&mut model, old);
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
        let Some(ModelEffect::Open { key: recovered, .. }) =
            model.fixture_validation_succeeded(&request)
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
        verify_open(&mut model, recovered);
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
        verify_open(&mut model, live);
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
        first.confirm_open_treatments();
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
        second.confirm_open_treatments();
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
        driver.confirm_open_treatments();
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
    fn muted_setter_without_engine_updates_local_preference_and_is_published() {
        let mut runtime = {
            let (mut runtime, _) = runtime();
            runtime.engine = None;
            runtime
        };
        runtime.poll();
        assert_eq!(runtime.set_muted(true), SubmitStatus::Accepted);
        let update = runtime.poll();
        assert!(update.changed && update.can_set_gain && update.muted);
        assert_eq!(update.playback_status, "Unavailable");
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
        settle_committed_watch(&mut runtime);
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
        settle_committed_watch(&mut runtime);
        let before = runtime.engine.as_ref().unwrap().model().state_identity();
        // Replace only the admission port. Keep the real owner alive, then
        // restore its port before cleanup; this is not a physical owner failure.
        let unavailable = GateRunner::with_spawner(fixture_treatment_capabilities(), |_, _| {
            panic!("no replacement opening requested")
        });
        let original =
            std::mem::replace(runtime.engine.as_mut().unwrap().runner_mut(), unavailable);
        let rejected = runtime.qualification_command("pause 1");
        *runtime.engine.as_mut().unwrap().runner_mut() = original;
        assert!(rejected.changed);
        assert!(!runtime.command_error.is_empty());
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
                    let (mut runtime, drivers) =
                        runtime_fixture(validate, restored.then_some(2), None);
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
                        restored_owner.confirm_open_treatments();
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
        let snapshot = session_fixture(&["/dev/video0"], request.settings.video.fixture_mode());
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
        let runner = GateRunner::with_spawner(
            fixture_treatment_capabilities(),
            move |generation, config| {
                let requested = config.video.requested();
                let input = config
                    .video
                    .validate_snapshot(&session_fixture(&["/dev/video0"], requested.mode))
                    .unwrap();
                let requested = input.requested().clone();
                let (driver, backend) = Driver::pair(Config::default());
                let backend = backend.with_open_treatments(config.treatment_key, config.treatments);
                tx.send((driver, config.watch)).unwrap();
                OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || backend)
            },
        );
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
            persistence: {
                let mut session = PersistenceSession::load(Err("test has no store".into()));
                session.prepare_startup(&StartupSelection {
                    restore_profile: false,
                    source_binding: None,
                    topology_pin: None,
                    adapt_video: false,
                    draft: Some(settings()),
                    auto_open: false,
                    reason: String::new(),
                    audio_policy: crate::app::settings::StartupAudioPolicy::Explicit,
                    bootstrap_audio: Default::default(),
                });
                session
            },
            auto_open: false,
            startup_apply: None,
            output: OutputPolicy::new(PersistentOutputChoice::default()),
            catalog: None,
            // Tests never touch a real Pulse connection.
            catalog_start_failed: true,
            registered_target: None,
            fed_plan_revision: None,
            last_catalog: None,
            last_rows: Vec::new(),
            last_output: None,
            catalog_stopping: false,
            catalog_join_error: None,
            ui: ProductUi::default(),
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
        let generation = Generation::new(opening.generation).unwrap();
        runtime.surface_ready(SurfaceToken {
            generation,
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
        let attempt = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .state_identity()
            .attempt()
            .expect("open attempt exists");
        // M3 loopback receipt test constructor: the legacy Pulse route receipt
        // is gone; Active audio carries the owned-loopback receipt with a
        // confirmed actual destination target from the desired plan.
        let identity = crate::domain::output::SinkIdentity::new(
            "test-output".to_owned(),
            vec![("device.api".to_owned(), "pipewire".to_owned())],
        )
        .unwrap();
        let live_target = crate::domain::output::LiveSinkTarget::new(
            identity,
            std::num::NonZeroU64::new(4242).unwrap(),
            17,
        )
        .unwrap();
        // The runtime has already fed its policy plan, so the fixture must
        // advance past the engine's actual current output revision; a stale
        // first() revision is rejected as StaleState.
        let revision = runtime
            .engine
            .as_ref()
            .unwrap()
            .output_plan()
            .revision()
            .next();
        runtime
            .engine
            .as_mut()
            .unwrap()
            .set_output_plan(crate::domain::output::OutputPlan::Target {
                revision,
                target: live_target.clone(),
            })
            .unwrap();
        // Drain the output change before the helper expects the pause transaction.
        let (_, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("accepted output change reaches the backend");
        assert!(matches!(
            command,
            crate::media::controller::BackendCommand::SetOutput(_)
        ));
        let destination = runtime
            .engine
            .as_ref()
            .unwrap()
            .output_plan()
            .target()
            .cloned()
            .expect("desired plan carries the live target");
        let route = crate::media::loopback::LoopbackReceipt::for_test(
            generation,
            attempt,
            epoch,
            watch,
            source_a.clone(),
            destination,
            revision,
        );
        driver.send(BackendEvent::AudioAvailability(AudioAvailability::Active {
            route: route.clone(),
        }));
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::PlaybackRestart);
        driver.confirm_open_treatments();
        driver.fence();
        let active = await_update(&mut runtime, |update| update.can_toggle_pause);
        assert_eq!(
            runtime.engine.as_ref().unwrap().audio_availability(),
            Some(&AudioAvailability::Active {
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
        runtime.set_stats_hidden(true);
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
        driver.confirm_open_treatments();
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
        assert!(final_update.stats_hidden);
        assert!(final_update.context_diagnostic.contains("surface_lost"));
        assert!(!final_update.can_open && !final_update.can_restart);
    }

    #[test]
    fn preferences_are_admitted_and_projected_without_engine_then_saved_at_safe_close() {
        let temp =
            std::env::temp_dir().join(format!("furami-runtime-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let path = temp.join("settings.json");
        let _ = std::fs::remove_file(&path);
        let persistence = PersistenceSession::load(Ok(path.clone()));
        let startup = StartupSelection {
            restore_profile: false,
            source_binding: None,
            topology_pin: None,
            adapt_video: false,
            draft: None,
            auto_open: false,
            reason: "Preferences only".into(),
            audio_policy: crate::app::settings::StartupAudioPolicy::Explicit,
            bootstrap_audio: Default::default(),
        };
        let mut runtime = RuntimeCoordinator::new(
            String::new(),
            fixture_treatment_capabilities(),
            startup,
            persistence,
            vec![],
        );
        let initial = runtime.poll();
        assert!(initial.can_set_gain && !initial.can_open && !initial.create_native);
        assert!(!initial.stats_hidden);
        assert!(runtime.ui.snapshot.as_ref().unwrap().stream_stats.is_none());
        let hidden = runtime.set_stats_hidden(true);
        assert!(hidden.changed && hidden.stats_hidden && !hidden.create_native);
        assert_eq!(hidden.context_diagnostic, initial.context_diagnostic);
        assert_eq!(runtime.set_volume(27), SubmitStatus::Accepted);
        assert_eq!(runtime.set_muted(true), SubmitStatus::Accepted);
        let update = runtime.set_fullscreen(true);
        assert_eq!(update.volume_percent, 27);
        assert!(update.muted && update.fullscreen);
        assert_eq!(runtime.set_volume(101), SubmitStatus::NotReady);
        assert_eq!(runtime.poll().volume_percent, 27);
        let close = runtime.request_application_close();
        assert!(!close.release_native);
        // begin_shutdown stops the application-lifetime catalog worker; quit is
        // only authorized after that worker actually joins, so poll until the
        // real barrier opens instead of assuming an immediate quit.
        let final_update = if close.quit {
            close
        } else {
            await_update(&mut runtime, |update| update.quit)
        };
        assert!(final_update.quit);
        assert!(runtime.set_stats_hidden(false).stats_hidden);
        let reloaded = PersistenceSession::load(Ok(path));
        assert_eq!(
            reloaded.preferences().gain,
            PlaybackGain::new(27, true).unwrap()
        );
        assert!(reloaded.preferences().fullscreen && reloaded.saved_selection().is_none());
        assert!(reloaded.preferences().stats_hidden);
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn close_with_internal_candidate_drains_owner_without_dirty_confirmation() {
        let (mut runtime, drivers) = runtime();
        let driver = start_live(&mut runtime, &drivers);
        runtime.qualification_command("draft-video 0 YUYV 1920x1080 30/1");
        let close = runtime.request_application_close();
        assert_ne!(close.close_dialog, "draft");
        assert!(!close.quit);
        let retired = if close.release_native {
            close
        } else {
            await_update(&mut runtime, |update| update.release_native)
        };
        assert!(!retired.quit);
        driver.destroyed.recv().unwrap();
        let drained = runtime.native_released(AttemptId::new(retired.generation).unwrap());
        let drained = if drained.close_dialog == "save" {
            drained
        } else {
            await_update(&mut runtime, |update| update.close_dialog == "save")
        };
        assert!(!drained.quit && !drained.can_open && !drained.can_set_gain);
        assert_eq!(runtime.set_volume(22), SubmitStatus::Closing);
        assert!(runtime.close_without_save().quit);
        assert!(drivers.try_recv().is_err());
    }

    #[test]
    fn startup_restore_opens_once_and_never_authorizes_applied_file_write() {
        use std::os::unix::fs::MetadataExt;
        for chain in [
            crate::domain::filters::FilterChain::default(),
            treatment_chain(false),
            treatment_chain(true),
        ] {
            let store = WritableSettings::new();
            let (mut prior, prior_drivers) = runtime();
            store.bind(&mut prior);
            edit_filters(&mut prior, &chain);
            let prior_driver = start_live(&mut prior, &prior_drivers);
            cleanup(&mut prior);
            prior_driver.destroyed.recv().unwrap();
            let original = std::fs::read(store.path()).unwrap();
            let inode = std::fs::metadata(store.path()).unwrap().ino();
            let saved = store.applied();
            assert_eq!(saved.filters, chain);

            let (mut runtime, drivers) = runtime();
            runtime
                .engine
                .as_mut()
                .unwrap()
                .edit_draft(DraftRevision::new(0), saved.clone())
                .unwrap();
            runtime.persistence = PersistenceSession::load(Ok(store.path()));
            runtime.persistence.prepare_startup(&StartupSelection {
                restore_profile: false,
                source_binding: None,
                topology_pin: None,
                adapt_video: false,
                draft: Some(saved),
                auto_open: true,
                reason: String::new(),
                audio_policy: crate::app::settings::StartupAudioPolicy::Explicit,
                bootstrap_audio: Default::default(),
            });
            runtime.auto_open = true;
            let opening = runtime.ui_ready();
            let opening = if opening.create_native {
                opening
            } else {
                await_update(&mut runtime, |update| update.create_native)
            };
            assert!(runtime.startup_apply.is_some());
            assert_eq!(runtime.persistence.authorized_apply(), None);
            let driver = drivers.recv().unwrap();
            let again = runtime.ui_ready();
            assert!(!again.create_native && drivers.try_recv().is_err());
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
            driver.confirm_open_treatments();
            driver.fence();
            let restored = await_update(&mut runtime, |update| update.can_toggle_pause);
            assert!(!restored.failed);
            assert!(runtime.startup_apply.is_none());
            assert_eq!(runtime.persistence.authorized_apply(), None);
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
                    .filters,
                chain
            );
            assert_eq!(std::fs::read(store.path()).unwrap(), original);
            assert_eq!(
                std::fs::metadata(store.path()).unwrap().ino(),
                inode,
                "startup must not rewrite even identical bytes"
            );
            assert!(!runtime.ui_ready().create_native && drivers.try_recv().is_err());
            cleanup(&mut runtime);
            driver.destroyed.recv().unwrap();
        }
    }

    fn observed_sink(
        name: &str,
        serial: u64,
        index: u32,
    ) -> crate::domain::output::SinkObservation {
        let identity = crate::domain::output::SinkIdentity::new(name.to_owned(), vec![]).unwrap();
        crate::domain::output::SinkObservation {
            target: crate::domain::output::LiveSinkTarget::new(
                identity,
                serial.try_into().unwrap(),
                index,
            )
            .unwrap(),
            description: format!("{name} description"),
            eligible: true,
        }
    }

    #[test]
    fn output_rows_are_json_objects_with_reserved_auto_key_and_scoped_sink_keys() {
        let (mut runtime, _drivers) = runtime();
        runtime.last_catalog = Some(crate::domain::output::SinkCatalog {
            revision: 7,
            sinks: vec![
                observed_sink("auto", 10, 3),
                observed_sink("speakers", 11, 4),
            ],
            default_sink: None,
        });
        let projection = runtime.output_projection();
        let parsed: serde_json::Value = serde_json::from_str(&projection.rows).unwrap();
        let rows = parsed.as_array().expect("rows are a JSON array");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["key"], "auto");
        assert_eq!(rows[0]["label"], "Auto");
        // Sink keys are revision-scoped and opaque: a sink literally named
        // "auto" can never collide with the reserved Auto row.
        assert_eq!(rows[1]["key"], "sink:7:0");
        assert_eq!(rows[2]["key"], "sink:7:1");
        assert_ne!(rows[1]["key"], "auto");
        // The runtime-side identity is never serialized into the projection.
        assert_eq!(rows[1].as_object().unwrap().len(), 4);
        // Selecting the reserved Auto key resolves to the Auto choice and
        // commits only through the accepted engine path. Every action must
        // echo the projected catalog revision: a stale revision is refused
        // with its actual diagnosis.
        let before = runtime.output.revision();
        assert_eq!(
            runtime.select_output("sink:7:9", 7),
            SubmitStatus::NotReady,
            "unknown keys never resolve to a choice"
        );
        assert!(runtime.command_error.contains("not in the current catalog"));
        assert_eq!(runtime.output.revision(), before);
        assert_eq!(runtime.select_output("auto", 7), SubmitStatus::Accepted);
        assert!(runtime.command_error.is_empty());
        assert!(runtime.output.revision().get() > before.get());
        assert_eq!(
            runtime.persistence.preferences().output,
            PersistentOutputChoice::Auto
        );
    }

    #[test]
    fn stale_output_selection_is_rejected_without_policy_mutation() {
        let (mut runtime, _drivers) = runtime();
        runtime.last_catalog = Some(crate::domain::output::SinkCatalog {
            revision: 3,
            sinks: vec![observed_sink("speakers", 5, 1)],
            default_sink: None,
        });
        runtime.output_projection();
        let before_revision = runtime.output.revision();
        let before_choice = runtime.output.choice().clone();
        // The user picked from rows that have since been replaced: the
        // decision is refused and the desired policy stays untouched.
        assert_eq!(
            runtime.select_output("sink:3:0", 2),
            SubmitStatus::StaleGeneration
        );
        assert!(runtime.command_error.contains("stale output selection"));
        assert_eq!(runtime.output.revision(), before_revision);
        assert_eq!(runtime.output.choice(), &before_choice);
        // The fresh projection is not shifted onto stale row indexes: the
        // new revision-scoped key of the same sink resolves cleanly.
        runtime.last_catalog.as_mut().unwrap().revision = 4;
        runtime.output_projection();
        assert_eq!(runtime.select_output("sink:4:0", 4), SubmitStatus::Accepted);
        match runtime.output.choice() {
            PersistentOutputChoice::Manual(identity) => {
                assert_eq!(identity.name(), "speakers");
            }
            other => panic!("expected manual choice, got {other:?}"),
        }
        match &runtime.persistence.preferences().output {
            PersistentOutputChoice::Manual(identity) => {
                assert_eq!(identity.name(), "speakers");
            }
            other => panic!("expected persisted manual choice, got {other:?}"),
        }
    }

    #[test]
    fn missing_manual_choice_stays_selected_and_visible_without_auto_fallback() {
        let (mut runtime, _drivers) = runtime();
        let ghost = crate::domain::output::SinkIdentity::new("ghost".to_owned(), vec![]).unwrap();
        runtime.output = OutputPolicy::new(PersistentOutputChoice::Manual(ghost));
        runtime.last_catalog = Some(crate::domain::output::SinkCatalog {
            revision: 2,
            sinks: vec![observed_sink("speakers", 5, 1)],
            default_sink: None,
        });
        let projection = runtime.output_projection();
        assert_eq!(projection.selected, "ghost");
        // No unique compatible row: the action key stays empty, which the UI
        // renders as selected-but-unavailable (index -1, never Auto).
        assert_eq!(projection.selected_key, "");
        let parsed: serde_json::Value = serde_json::from_str(&projection.rows).unwrap();
        // The unavailable saved choice is absent from the rows, so the UI
        // keeps it selected-but-unavailable instead of silently mapping it
        // onto the Auto row.
        assert!(
            parsed
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["name"] != "ghost")
        );
        // A unique compatible observed row projects its revision-scoped key;
        // after a catalog change the key is re-scoped, never a shifted index.
        let speakers =
            crate::domain::output::SinkIdentity::new("speakers".to_owned(), vec![]).unwrap();
        runtime.output = OutputPolicy::new(PersistentOutputChoice::Manual(speakers));
        let projection = runtime.output_projection();
        assert_eq!(projection.selected_key, "sink:2:0");
        runtime.last_catalog.as_mut().unwrap().revision = 3;
        let projection = runtime.output_projection();
        assert_eq!(projection.selected_key, "sink:3:0");
    }

    #[test]
    fn close_awaits_catalog_retirement_and_authorizes_quit_with_changed_update() {
        let (mut runtime, _drivers) = runtime();
        runtime.engine = None;
        // A real catalog worker held behind a delayed-release barrier (M3
        // test constructor): the first try_join calls find the worker still
        // running, then the actual retirement joins. No exact poll count is
        // asserted; the barrier release is awaited.
        runtime.catalog = Some(OutputCatalogWatch::for_test_delayed_releases(2));
        runtime.catalog_start_failed = false;
        runtime.last_catalog = Some(crate::domain::output::SinkCatalog {
            revision: 1,
            sinks: vec![observed_sink("speakers", 5, 1)],
            default_sink: None,
        });
        runtime.output_projection();
        let first = runtime.request_application_close();
        assert!(
            !first.quit,
            "a still-running worker never opens the barrier"
        );
        let intermediate = runtime.poll();
        assert!(
            !intermediate.quit,
            "the catalog worker is still held behind its release barrier"
        );
        let drained = await_update(&mut runtime, |update| {
            update.quit || update.close_dialog == "save"
        });
        let quitting = if drained.quit {
            drained
        } else {
            let discarded = runtime.close_without_save();
            if discarded.quit {
                discarded
            } else {
                await_update(&mut runtime, |update| update.quit)
            }
        };
        assert!(quitting.quit, "catalog retirement must authorize quit");
        assert!(quitting.changed, "quit authorization must set changed");
        assert!(runtime.catalog_join_error.is_none());
    }

    fn renderer_settings_with_interpolation(
        interpolation: bool,
    ) -> crate::domain::render::RendererSettings {
        let mut values = crate::domain::render::RendererSettings::default().values();
        values.interpolation = interpolation;
        crate::domain::render::RendererSettings::new(values).unwrap()
    }

    #[test]
    fn runtime_renderer_edit_is_stored_in_the_draft_until_an_explicit_apply() {
        let (mut runtime, drivers) = runtime();
        let before = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .revision
            .get();
        let requested = renderer_settings_with_interpolation(true);
        let update = runtime.qualification_command(&format!(
            "draft-renderer {before} {}",
            serde_json::to_string(&requested).unwrap(),
        ));
        assert!(update.changed);
        assert!(
            update.diagnostic.contains(INTERPOLATION_INACTIVE_TEXT),
            "interpolation notice must ride the existing bounded diagnostic bridge: {}",
            update.diagnostic
        );
        {
            let model = runtime.engine.as_ref().unwrap().model();
            assert_eq!(model.draft().settings.renderer, requested);
            assert!(model.draft().revision.get() > before);
            // Editing stores the request only: nothing is applied or opened.
            assert!(model.active().is_none());
            assert!(model.last_valid().is_none());
        }
        assert!(
            drivers.try_recv().is_err(),
            "a draft renderer edit never opens a backend owner"
        );
        let snapshot = runtime_snapshot(runtime.engine.as_mut().unwrap());
        assert_eq!(snapshot["draft"]["renderer"], serde_json::json!(&requested));
        assert_eq!(snapshot["active_settings"], serde_json::Value::Null);
        assert_eq!(snapshot["treatments"], serde_json::Value::Null);
        // A stale revision is refused without moving the stored draft.
        let stale = runtime.qualification_command(&format!(
            "draft-renderer {before} {}",
            serde_json::to_string(&renderer_settings_with_interpolation(false)).unwrap(),
        ));
        assert!(
            stale.diagnostic.contains("draft revision is stale"),
            "stale revision must be refused: {}",
            stale.diagnostic
        );
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .renderer,
            requested
        );
    }

    fn product_runtime() -> (RuntimeCoordinator, mpsc::Receiver<Driver>) {
        product_runtime_with_validator(validate_product)
    }
    fn product_runtime_with_validator(
        execute: fn(
            ValidationRequest,
        ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>),
    ) -> (RuntimeCoordinator, mpsc::Receiver<Driver>) {
        let (mut runtime, drivers) = runtime_fixture(
            execute,
            None,
            Some(session_fixture(
                &["/dev/video0"],
                settings().video.fixture_mode(),
            )),
        );
        let capabilities = fixture_treatment_capabilities();
        (runtime.ui.filters, runtime.ui.renderer) = projection::treatments(&capabilities);
        runtime.ui.catalog = capture_catalog::Model::fixture(
            session_fixture(&["/dev/video0"], settings().video.fixture_mode()),
            vec![crate::capture::audio::AudioSource {
                identity: runtime.sources[0].clone(),
                description: "Explicit test input".into(),
            }],
        );
        runtime.poll();
        (runtime, drivers)
    }
    fn product_values(runtime: &mut RuntimeCoordinator) -> serde_json::Value {
        runtime.dirty = true;
        let update = runtime.poll();
        if update.ui_snapshot.is_empty() {
            serde_json::to_value(runtime.ui.snapshot.as_ref().unwrap()).unwrap()
        } else {
            serde_json::from_str(&update.ui_snapshot).unwrap()
        }
    }
    #[test]
    fn product_revisions_keep_u64_precision_and_never_wrap() {
        let (mut runtime, _) = product_runtime();
        runtime.ui.public_revision = 9_007_199_254_740_993;
        runtime.ui.state_sequence = u64::MAX;
        let values = product_values(&mut runtime);
        assert_eq!(
            values["selectionRevision"].as_str(),
            Some("9007199254740993")
        );
        assert_eq!(
            values["stateToken"].as_str(),
            Some("state:18446744073709551615")
        );
        runtime.ui.public_revision = u64::MAX;
        let before = runtime.engine.as_ref().unwrap().model().draft().clone();
        runtime.set_interpolation(&u64::MAX.to_string(), true);
        assert_eq!(runtime.ui.public_revision, u64::MAX);
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &before);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "unavailable");
        cleanup(&mut runtime);
    }
    #[test]
    fn product_stale_selection_and_catalog_are_refused_without_mutation() {
        let (mut runtime, drivers) = product_runtime();
        let before = runtime.engine.as_ref().unwrap().model().draft().clone();
        let revision = runtime.ui.public_revision;
        let catalog = runtime.ui.catalog.revision.to_string();
        runtime.set_rate("0", &catalog, "30000/1001");
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "stale_selection");
        runtime.set_size(&revision.to_string(), "0", "1920", "1080");
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "stale_catalog");
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &before);
        assert_eq!(runtime.ui.public_revision, revision);
        assert!(drivers.try_recv().is_err());
        cleanup(&mut runtime);
    }
    #[test]
    fn incomplete_bootstrap_projects_null_selection_and_refuses_non_source_edits() {
        let (mut runtime, _) = product_runtime();
        // A complete prepared engine remains usable before its row key exists.
        runtime.ui.selected = None;
        runtime.ui.catalog = capture_catalog::Model::default();
        let prepared = product_values(&mut runtime);
        assert!(prepared["selection"].is_object());
        assert!(prepared["selection"]["size"].is_object());
        assert!(prepared["selection"]["rate"].is_object());
        assert_eq!(prepared["selection"]["sourceKey"], "");
        assert!(runtime.ui.snapshot.as_ref().unwrap().can_edit_selection);
        runtime.engine = None;
        runtime.ui.bootstrap_runner = Some(GateRunner::new(
            String::new(),
            fixture_treatment_capabilities(),
        ));
        runtime.ui.catalog = capture_catalog::Model::fixture(
            session_fixture(&["/dev/video0"], settings().video.fixture_mode()),
            Vec::new(),
        );
        runtime.ui.domain_revision = None;
        for has_partial_source in [false, true] {
            let mut partial = PartialDraft::default();
            if has_partial_source {
                partial.source = Some(runtime.ui.catalog.sources[0].physical());
                partial.size = Some(settings().video.mode.size);
            }
            runtime.ui.partial = Some(partial.clone());
            let values = product_values(&mut runtime);
            assert!(values["selection"].is_null());
            assert!(values["canEditSelection"].as_bool().unwrap());
            assert!(runtime.ui.catalog.projection.source_rows[0].eligible);
            let revision = runtime.ui.public_revision;
            runtime.set_audio_enabled(&revision.to_string(), false);
            assert_eq!(runtime.ui.error.as_ref().unwrap().code, "invalid_value");
            runtime.set_interpolation(&revision.to_string(), true);
            assert_eq!(runtime.ui.error.as_ref().unwrap().code, "invalid_value");
            assert_eq!(runtime.ui.public_revision, revision);
            assert_eq!(runtime.ui.partial.as_ref(), Some(&partial));
            assert!(runtime.engine.is_none());
            assert!(runtime.ui.submitted.is_none());
        }
        cleanup(&mut runtime);
    }

    #[test]
    fn source_only_bootstrap_installs_complete_mode_and_submits_once() {
        let (mut runtime, _) = product_runtime();
        runtime.engine = None;
        runtime.ui.bootstrap_runner = Some(GateRunner::new(
            String::new(),
            fixture_treatment_capabilities(),
        ));
        runtime.ui.partial = Some(PartialDraft::default());
        runtime.ui.selected = None;
        runtime.ui.domain_revision = None;
        runtime.ui.state_initialized = false;
        runtime.ui.snapshot = None;
        let first = product_values(&mut runtime);
        assert_eq!(first["schema"], 2);
        assert!(first["canEditSelection"].as_bool().unwrap());
        assert!(first["selection"].is_null());
        let revision = runtime.ui.public_revision;
        let source = runtime.ui.catalog.projection.source_rows[0].key.clone();
        let catalog = runtime.ui.catalog.revision.to_string();
        runtime.select_capture(&revision.to_string(), &catalog, &source);
        let model = runtime
            .engine
            .as_ref()
            .expect("source alone creates complete engine")
            .model();
        assert_eq!(
            model.draft().settings.video.mode.size,
            settings().video.mode.size
        );
        assert_eq!(
            model.draft().settings.video.mode.rate,
            settings().video.mode.rate
        );
        assert_eq!(
            model.draft().settings.video.mode.format_policy,
            InputFormatPolicy::Auto
        );
        assert!(runtime.ui.partial.is_none());
        assert_eq!(runtime.ui.public_revision, revision + 1);
        assert!(runtime.ui.submitted.is_some());
        let submitted = runtime.ui.submitted.clone();
        runtime.poll();
        assert_eq!(runtime.ui.public_revision, revision + 1);
        assert_eq!(runtime.ui.submitted, submitted);
        cleanup(&mut runtime);
    }
    #[test]
    fn immediate_renderer_commits_once_without_reopening_and_busy_inputs_cannot_stage() {
        let (mut runtime, drivers) = product_runtime();
        let store = WritableSettings::new();
        store.bind(&mut runtime);
        let driver = start_live(&mut runtime, &drivers);
        let revision = runtime.ui.public_revision;
        runtime.set_interpolation(&revision.to_string(), true);
        let requested = runtime.engine.as_ref().unwrap().model().draft().clone();
        assert!(requested.settings.renderer.values().interpolation);
        assert_eq!(runtime.ui.public_revision, revision + 1);
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let crate::media::controller::BackendCommand::ApplyTreatments { key, .. } = command else {
            panic!("same-source renderer edit must use the existing treatment owner");
        };
        let selected = runtime.ui.selected.clone();
        let policy = runtime.ui.audio_policy;
        runtime.set_audio_enabled(&runtime.ui.public_revision.to_string(), false);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "busy");
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &requested);
        assert_eq!(runtime.ui.selected, selected);
        assert_eq!(runtime.ui.audio_policy, policy);
        assert_eq!(runtime.ui.public_revision, revision + 1);
        assert!(!runtime.ui.snapshot.as_ref().unwrap().can_edit_selection);
        // The submitted prepared treatments are compiled from the accepted
        // candidate, so the proof carries that exact renderer without reading
        // controller-private payload internals.
        confirm_unchanged_filter_treatments(
            &runtime,
            &driver,
            id,
            key,
            requested.settings.renderer.clone(),
        );
        await_update(&mut runtime, |update| update.product_phase == "Active");
        assert_eq!(store.applied().renderer, requested.settings.renderer);
        assert!(
            drivers.try_recv().is_err(),
            "treatment must not open a second owner"
        );
        assert!(
            driver.submitted.try_recv().is_err(),
            "refused input cannot replay"
        );
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn loaded_repeated_filters_preserve_order_labels_and_parameters_on_individual_toggle() {
        use crate::domain::filters::{EqParams, EqValues, Filter};
        let (mut runtime, drivers) = product_runtime();
        let make = |brightness| {
            Filter::Eq(
                EqParams::new(EqValues {
                    brightness,
                    contrast: 1.25,
                    saturation: 0.75,
                    gamma: 1.4,
                    gamma_r: 1.1,
                    gamma_g: 0.9,
                    gamma_b: 1.2,
                    gamma_weight: 0.8,
                })
                .unwrap(),
            )
        };
        let chain = FilterChain::new(vec![
            FilterEntry::new("Loaded\nlabel".into(), make(0.2), true),
            FilterEntry::new("Second repeated eq".into(), make(-0.1), false),
        ])
        .unwrap();
        let engine = runtime.engine.as_mut().unwrap();
        let mut draft = engine.model().draft().settings.clone();
        draft.filters = chain.clone();
        engine
            .edit_draft(engine.model().draft().revision, draft)
            .unwrap();
        runtime.poll();
        // The immediate toggle is a real transaction: the owner it starts must
        // stay observable for the whole test instead of outliving its fixture.
        let driver = start_live(&mut runtime, &drivers);
        let revision = runtime.ui.public_revision.to_string();
        let entry = runtime.ui.snapshot.as_ref().unwrap().filters.entries[1]
            .key
            .clone();
        runtime.set_filter_enabled(&revision, &entry, true);
        let requested = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .settings
            .filters
            .clone();
        let entries = requested.entries();
        assert_eq!(entries[0], chain.entries()[0]);
        assert_eq!(entries[1].filter(), chain.entries()[1].filter());
        assert_eq!(entries[1].label(), chain.entries()[1].label());
        assert!(entries[1].enabled());
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let crate::media::controller::BackendCommand::ApplyTreatments { key, .. } = command else {
            panic!("a changed filter chain must use the treatment owner")
        };
        // Keys embed the admission revision: re-read the row key the same way
        // the real UI does after the accepted transaction advanced it.
        let entry = runtime.ui.snapshot.as_ref().unwrap().filters.entries[1]
            .key
            .clone();
        runtime.set_filter_enabled(&runtime.ui.public_revision.to_string(), &entry, false);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "busy");
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .filters,
            requested
        );
        runtime.add_filter(&runtime.ui.public_revision.to_string(), "eq");
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .filters,
            requested
        );
        confirm_treatments(&driver, id, key);
        await_update(&mut runtime, |update| update.product_phase == "Active");
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
                .filters,
            requested
        );
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn removing_a_filter_preserves_remaining_order_parameters_and_uses_real_transaction() {
        use crate::domain::filters::{EqParams, EqValues, Filter};
        let (mut runtime, drivers) = product_runtime();
        let make = |brightness| {
            Filter::Eq(
                EqParams::new(EqValues {
                    brightness,
                    contrast: 1.25,
                    saturation: 0.75,
                    gamma: 1.4,
                    gamma_r: 1.1,
                    gamma_g: 0.9,
                    gamma_b: 1.2,
                    gamma_weight: 0.8,
                })
                .unwrap(),
            )
        };
        let chain = FilterChain::new(vec![
            FilterEntry::new("First eq".into(), make(0.3), true),
            FilterEntry::new("Second eq".into(), make(-0.4), false),
        ])
        .unwrap();
        let engine = runtime.engine.as_mut().unwrap();
        let mut draft = engine.model().draft().settings.clone();
        draft.filters = chain.clone();
        engine
            .edit_draft(engine.model().draft().revision, draft)
            .unwrap();
        runtime.poll();
        // The removal is a real transaction: the owner it starts must stay
        // observable for the whole test instead of outliving its fixture.
        let driver = start_live(&mut runtime, &drivers);
        let revision = runtime.ui.public_revision.to_string();
        let entry = runtime.ui.snapshot.as_ref().unwrap().filters.entries[0]
            .key
            .clone();
        runtime.remove_filter(&revision, &entry);
        let requested = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .settings
            .filters
            .clone();
        assert_eq!(requested.entries().len(), 1);
        // The survivor keeps its position, label, enabled flag and exact parameters.
        assert_eq!(requested.entries()[0], chain.entries()[1]);
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let crate::media::controller::BackendCommand::ApplyTreatments { key, .. } = command else {
            panic!("a changed filter chain must use the treatment owner")
        };
        // While the transaction is busy a further removal of the survivor is
        // refused against the current revision without touching the draft.
        let entry = runtime.ui.snapshot.as_ref().unwrap().filters.entries[0]
            .key
            .clone();
        runtime.remove_filter(&runtime.ui.public_revision.to_string(), &entry);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "busy");
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .filters,
            requested
        );
        confirm_treatments(&driver, id, key);
        await_update(&mut runtime, |update| update.product_phase == "Active");
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
                .filters,
            requested
        );
        assert!(driver.submitted.try_recv().is_err());
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn removing_the_last_filter_yields_empty_chain_and_stale_key_is_refused() {
        use crate::domain::filters::{EqParams, EqValues, Filter};
        let (mut runtime, drivers) = product_runtime();
        let chain = FilterChain::new(vec![FilterEntry::new(
            "Only eq".into(),
            Filter::Eq(
                EqParams::new(EqValues {
                    contrast: 1.0,
                    brightness: 0.5,
                    saturation: 1.0,
                    gamma: 1.0,
                    gamma_r: 1.0,
                    gamma_g: 1.0,
                    gamma_b: 1.0,
                    gamma_weight: 1.0,
                })
                .unwrap(),
            ),
            true,
        )])
        .unwrap();
        let engine = runtime.engine.as_mut().unwrap();
        let mut draft = engine.model().draft().settings.clone();
        draft.filters = chain.clone();
        engine
            .edit_draft(engine.model().draft().revision, draft)
            .unwrap();
        runtime.poll();
        let driver = start_live(&mut runtime, &drivers);
        let revision = runtime.ui.public_revision.to_string();
        // An index beyond the chain inside the current revision is refused by
        // the domain before any mutation.
        runtime.remove_filter(&revision, &format!("filter:{revision}:7"));
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "filter_error");
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .filters,
            chain
        );
        // Removing the only entry leaves a valid, empty chain.
        let entry = runtime.ui.snapshot.as_ref().unwrap().filters.entries[0]
            .key
            .clone();
        runtime.remove_filter(&revision, &entry);
        let requested = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .settings
            .filters
            .clone();
        assert!(requested.entries().is_empty());
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let crate::media::controller::BackendCommand::ApplyTreatments { key, .. } = command else {
            panic!("a changed filter chain must use the treatment owner")
        };
        // The accepted transaction advanced the revision, so the superseded
        // key is refused as stale instead of removing the wrong entry.
        runtime.remove_filter(&revision, &entry);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "stale_selection");
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .filters,
            requested
        );
        confirm_treatments(&driver, id, key);
        await_update(&mut runtime, |update| update.product_phase == "Active");
        assert!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .active()
                .unwrap()
                .applied()
                .settings()
                .filters
                .entries()
                .is_empty()
        );
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn immediate_filter_confirmation_preserves_requested_chain_and_preferences() {
        let (mut runtime, drivers) = product_runtime();
        let driver = start_live(&mut runtime, &drivers);
        runtime.add_filter(&runtime.ui.public_revision.to_string(), "eq");
        let requested = runtime.engine.as_ref().unwrap().model().draft().clone();
        let revision = runtime.ui.public_revision;
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let crate::media::controller::BackendCommand::ApplyTreatments { key, .. } = command else {
            panic!("real immediate treatment admission required");
        };
        runtime.set_interpolation(&revision.to_string(), true);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "busy");
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &requested);
        runtime.set_volume(21);
        confirm_treatments(&driver, id, key);
        await_update(&mut runtime, |update| update.product_phase == "Active");
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
                .filters,
            requested.settings.filters
        );
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .filters,
            requested.settings.filters
        );
        assert_eq!(runtime.ui.public_revision, revision);
        assert_eq!(runtime.persistence.preferences().gain.volume_percent, 21);
        assert!(driver.submitted.try_recv().is_err());
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn refused_original_remains_refused_after_real_product_apply_and_preference_changes() {
        let store = WritableSettings::new();
        std::fs::write(store.path(), b"invalid original settings").unwrap();
        let original = std::fs::read(store.path()).unwrap();
        let (mut runtime, drivers) = product_runtime();
        runtime.persistence = PersistenceSession::load(Ok(store.path()));
        runtime.persistence.prepare_startup(&StartupSelection {
            restore_profile: false,
            source_binding: None,
            topology_pin: None,
            adapt_video: false,
            draft: Some(settings()),
            auto_open: false,
            reason: String::new(),
            audio_policy: crate::app::settings::StartupAudioPolicy::Explicit,
            bootstrap_audio: Default::default(),
        });
        let driver = start_live(&mut runtime, &drivers);
        runtime.add_filter(&runtime.ui.public_revision.to_string(), "eq");
        let (id, command) = driver
            .submitted
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let crate::media::controller::BackendCommand::ApplyTreatments { key, .. } = command else {
            panic!("real treatment admission required")
        };
        runtime.set_volume(29);
        confirm_treatments(&driver, id, key);
        await_update(&mut runtime, |update| update.product_phase == "Active");
        assert!(runtime.ui.snapshot.as_ref().unwrap().settings.refused);
        assert_eq!(std::fs::read(store.path()).unwrap(), original);
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn source_choice_without_any_auto_eligible_format_never_changes_candidate() {
        let (mut runtime, drivers) = product_runtime();
        let original = runtime.engine.as_ref().unwrap().model().draft().clone();
        let old_source = runtime.ui.catalog.sources[0].clone();
        let unsupported = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"BGR3"),
            size: FrameSize::new(640, 480).unwrap(),
            rate: FrameRate::new(30, 1).unwrap(),
        };
        runtime.ui.catalog = capture_catalog::Model::fixture(
            crate::capture::linux::physical_fixture(&[
                (
                    old_source.device.identity().clone(),
                    "/dev/video0",
                    old_source.node.usb_syspath().to_str().unwrap(),
                    old_source.node.syspath().to_str().unwrap(),
                    original.settings.video.fixture_mode(),
                ),
                (
                    old_source.device.identity().clone(),
                    "/dev/video-second",
                    old_source.node.usb_syspath().to_str().unwrap(),
                    "/sys/ui-fixture/second-node",
                    unsupported,
                ),
            ]),
            Vec::new(),
        );
        product_values(&mut runtime);
        let selected = runtime.ui.selected.clone();
        let revision = runtime.ui.public_revision;
        let key = runtime.ui.catalog.projection.source_rows[1].key.clone();
        runtime.select_capture(
            &revision.to_string(),
            &runtime.ui.catalog.revision.to_string(),
            &key,
        );
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "capture_error");
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &original);
        assert_eq!(runtime.ui.selected, selected);
        assert_eq!(runtime.ui.public_revision, revision);
        assert!(drivers.try_recv().is_err());
        cleanup(&mut runtime);
    }
    #[test]
    fn off_audio_retains_explicit_input_and_never_selects_the_first_input() {
        let (mut runtime, _) = product_runtime();
        let revision = runtime.ui.public_revision.to_string();
        runtime.set_audio_enabled(&revision, true);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "audio_error");
        assert!(
            !runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .audio
                .enabled()
        );
        let catalog = runtime.ui.catalog.revision.to_string();
        let key = runtime.ui.catalog.projection.audio_input_rows[0]
            .key
            .clone();
        runtime.select_audio_input(&revision, &catalog, &key);
        let selected = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .settings
            .audio
            .source()
            .unwrap()
            .clone();
        assert!(
            !runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .audio
                .enabled()
        );
        assert_eq!(
            runtime.ui.audio_policy,
            crate::app::settings::StartupAudioPolicy::Explicit
        );
        cleanup(&mut runtime);
        for enabled in [false, true] {
            let (mut runtime, _) = product_runtime();
            let engine = runtime.engine.as_mut().unwrap();
            let mut candidate = engine.model().draft().settings.clone();
            candidate.audio = AudioSelection::Disabled {
                retained: Some(selected.clone()),
            };
            engine
                .edit_draft(engine.model().draft().revision, candidate)
                .unwrap();
            runtime.poll();
            runtime.set_audio_enabled(&runtime.ui.public_revision.to_string(), enabled);
            assert_eq!(
                runtime
                    .engine
                    .as_ref()
                    .unwrap()
                    .model()
                    .draft()
                    .settings
                    .audio
                    .enabled(),
                enabled
            );
            assert_eq!(
                runtime
                    .engine
                    .as_ref()
                    .unwrap()
                    .model()
                    .draft()
                    .settings
                    .audio
                    .source(),
                Some(&selected)
            );
            assert!(runtime.ui.error.is_none());
            cleanup(&mut runtime);
        }
    }
    #[test]
    fn renderer_qualified_unsupported_and_inactive_values_remain_honest() {
        let (mut runtime, _) = product_runtime();
        let unavailable = runtime
            .ui
            .renderer
            .fields
            .iter()
            .find(|field| field.key == "scale")
            .unwrap()
            .choices
            .iter()
            .find(|choice| choice.key == "ewa_lanczossoft")
            .unwrap();
        assert!(!unavailable.eligible && !unavailable.reason.is_empty());
        let before = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .settings
            .renderer
            .clone();
        runtime.set_scale(&runtime.ui.public_revision.to_string(), "ewa_lanczossoft");
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .renderer,
            before
        );
        runtime.set_deband_iterations(&runtime.ui.public_revision.to_string(), -1);
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .renderer,
            before
        );
        runtime.set_scale_antiring(&runtime.ui.public_revision.to_string(), f64::NAN);
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .renderer,
            before
        );
        runtime.set_interpolation(&runtime.ui.public_revision.to_string(), true);
        let after = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .settings
            .renderer
            .values();
        let mut expected = before.values();
        expected.interpolation = true;
        assert_eq!(after, expected);
        assert!(
            !runtime
                .ui
                .snapshot
                .as_ref()
                .unwrap()
                .renderer
                .notice
                .is_empty()
        );
        assert!(
            runtime.ui.submitted.is_some(),
            "valid completion immediately submits the requested renderer"
        );
        cleanup(&mut runtime);
    }
    #[test]
    fn explicit_missing_source_is_not_overwritten_by_the_incumbent_pair() {
        let (mut runtime, drivers) = product_runtime();
        let driver = start_live(&mut runtime, &drivers);
        runtime.ui.catalog = capture_catalog::Model::fixture(
            session_fixture(
                &["/dev/video0", "/dev/video1"],
                settings().video.fixture_mode(),
            ),
            Vec::new(),
        );
        product_values(&mut runtime);
        let key = runtime.ui.catalog.projection.source_rows[1].key.clone();
        let catalog = runtime.ui.catalog.revision.to_string();
        runtime.select_capture(&runtime.ui.public_revision.to_string(), &catalog, &key);
        let pending = runtime.engine.as_ref().unwrap().model().draft().clone();
        let chosen = runtime.ui.selected.clone();
        runtime.ui.catalog = capture_catalog::Model::fixture(
            session_fixture(&["/dev/video0"], settings().video.fixture_mode()),
            Vec::new(),
        );
        product_values(&mut runtime);
        runtime.set_interpolation(&runtime.ui.public_revision.to_string(), true);
        assert!(matches!(
            runtime.ui.error.as_ref().unwrap().code,
            "busy" | "stale_catalog"
        ));
        assert_eq!(runtime.ui.selected, chosen);
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &pending);
        assert!(runtime.engine.as_ref().unwrap().model().active().is_some());
        assert!(drivers.try_recv().is_err());
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn applied_projection_does_not_label_a_missing_node_as_a_same_device_sibling() {
        let (mut runtime, drivers) = product_runtime();
        let driver = start_live(&mut runtime, &drivers);
        let original_node = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .active()
            .unwrap()
            .applied()
            .physical_node()
            .unwrap()
            .0
            .to_owned();
        runtime.ui.catalog = capture_catalog::Model::fixture(
            session_fixture(&["/dev/video1"], settings().video.fixture_mode()),
            Vec::new(),
        );
        product_values(&mut runtime);
        let applied = runtime
            .ui
            .snapshot
            .as_ref()
            .unwrap()
            .applied
            .as_ref()
            .unwrap();
        assert!(applied.source_key.is_empty());
        assert!(!applied.source_available);
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .active()
                .unwrap()
                .applied()
                .physical_node()
                .unwrap()
                .0,
            original_node.as_path()
        );
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn source_selection_pins_current_physical_route_with_and_without_partial_bootstrap() {
        fn observe_candidate(
            request: ValidationRequest,
        ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>) {
            let failure = ApplyFailure::new(
                crate::domain::failure::FailureCategory::Validation(
                    crate::domain::failure::ValidationLayer::Discovery,
                ),
                crate::domain::failure::Stage::Prevalidation,
                crate::domain::failure::Cause::Generic,
                request.settings.clone(),
                "observed_candidate",
                format!("{:?}", request.pinned_node),
            );
            (request, Err(failure))
        }
        for partial in [false, true] {
            let mode = settings().video.fixture_mode();
            let identity = settings().video.identity;
            let snapshot = crate::capture::linux::physical_fixture(&[(
                identity.clone(),
                "/dev/video1",
                "/sys/usb/chosen",
                "/sys/node/chosen",
                mode,
            )]);
            let (mut runtime, drivers) =
                runtime_fixture(observe_candidate, None, Some(snapshot.clone()));
            runtime.ui.catalog = capture_catalog::Model::fixture(snapshot, Vec::new());
            product_values(&mut runtime);
            if partial {
                runtime.ui.partial = Some(PartialDraft {
                    size: None,
                    rate: None,
                    ..runtime.current_partial()
                });
            }
            let key = runtime.ui.catalog.projection.source_rows[0].key.clone();
            runtime.select_capture(
                &runtime.ui.public_revision.to_string(),
                &runtime.ui.catalog.revision.to_string(),
                &key,
            );
            assert!(runtime.ui.submitted.is_some());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .validation_rejection()
                .is_none()
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "candidate was not observed"
                );
                runtime.poll();
                std::thread::yield_now();
            }
            let failure = &runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .validation_rejection()
                .unwrap()
                .failure;
            assert_eq!(failure.operation, "observed_candidate");
            assert!(failure.requested.video.selected_node.is_none());
            assert!(
                failure.diagnostic.contains("/sys/node/chosen")
                    && failure.diagnostic.contains("/sys/usb/chosen")
            );
            let failure_text = failure.to_string();
            let hidden = runtime.set_stats_hidden(true);
            assert!(hidden.stats_hidden);
            assert!(hidden.context_diagnostic.contains(&failure_text));
            let shown = runtime.set_stats_hidden(false);
            assert_eq!(shown.context_diagnostic, hidden.context_diagnostic);
            cleanup(&mut runtime);
            assert!(drivers.try_recv().is_err());
        }
    }
    // These admission probes inspect rejection before allowing the already
    // admitted legitimate validation to complete. Fast fixture validation
    // must not race their strict no-owner assertion.
    static ADMISSION_PROBE_VALIDATION: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    fn validate_after_admission_probe(
        request: ValidationRequest,
    ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>) {
        let _guard = ADMISSION_PROBE_VALIDATION.lock();
        validate_product(request)
    }

    #[test]
    fn rejected_immediate_selection_does_not_install_or_revise_candidate() {
        for partial in [false, true] {
            let (mut runtime, drivers) =
                product_runtime_with_validator(validate_after_admission_probe);
            // Declared after runtime so unwinding releases validation before
            // RuntimeCoordinator's shutdown joins its worker.
            let validation_guard = ADMISSION_PROBE_VALIDATION.lock();
            let engine = runtime.engine.as_mut().unwrap();
            engine
                .apply(
                    engine.model().state_identity(),
                    engine.model().draft().revision,
                )
                .unwrap();
            product_values(&mut runtime);
            if partial {
                let mut staged = runtime.current_partial();
                staged.renderer = renderer_settings_with_interpolation(true);
                runtime.ui.partial = Some(staged);
            }
            let before = runtime.engine.as_ref().unwrap().model().draft().clone();
            let revision = runtime.ui.public_revision;
            runtime.set_interpolation(&revision.to_string(), true);
            assert_eq!(runtime.ui.error.as_ref().unwrap().code, "busy");
            assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &before);
            assert_eq!(runtime.ui.public_revision, revision);
            runtime.quit();
            drop(validation_guard);
            cleanup(&mut runtime);
            assert!(drivers.try_recv().is_err());
        }
    }
    #[test]
    fn application_close_supersedes_reset_without_saving_a_candidate() {
        let store = WritableSettings::new();
        std::fs::write(store.path(), b"invalid original settings").unwrap();
        let (mut runtime, _) = product_runtime();
        store.bind(&mut runtime);
        let original = std::fs::read(store.path()).unwrap();
        runtime.request_reset();
        let token = runtime.persistence.reset_token();
        assert_ne!(token, 0);
        let close = runtime.request_application_close();
        assert_ne!(close.close_dialog, "draft");
        assert_eq!(runtime.persistence.reset_token(), 0);
        assert!(!runtime.persistence.mutations_open());
        runtime.decide_reset(token, true);
        assert_eq!(std::fs::read(store.path()).unwrap(), original);
        cleanup(&mut runtime);
    }
    #[test]
    fn empty_bootstrap_closes_without_dirty_dialog_and_blocks_late_selection() {
        let (mut runtime, _) = product_runtime();
        runtime.engine = None;
        runtime.ui.partial = Some(PartialDraft::default());
        runtime.ui.selected = None;
        runtime.ui.domain_revision = None;
        runtime.ui.state_initialized = false;
        runtime.ui.snapshot = None;
        product_values(&mut runtime);
        let revision = runtime.ui.public_revision;
        let key = runtime.ui.catalog.projection.source_rows[0].key.clone();
        let catalog = runtime.ui.catalog.revision.to_string();
        let close = runtime.request_application_close();
        assert_ne!(close.close_dialog, "draft");
        runtime.select_capture(&revision.to_string(), &catalog, &key);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "closing");
        assert_eq!(runtime.current_partial(), PartialDraft::default());
        assert_eq!(runtime.ui.public_revision, revision);
        assert!(!runtime.ui.snapshot.as_ref().unwrap().can_edit_selection);
        cleanup(&mut runtime);
    }

    fn pending_source_fixture(
        runtime: &mut RuntimeCoordinator,
        serial: u64,
    ) -> capture_catalog::Physical {
        let physical = runtime.ui.catalog.sources[0].physical();
        let mut partial = runtime.current_partial();
        partial.source = Some(physical.clone());
        runtime.ui.resolving = Some(PendingSelection {
            serial,
            catalog_revision: runtime.ui.catalog.revision,
            selection_revision: runtime.ui.public_revision,
            partial,
            domain_revision: runtime
                .engine
                .as_ref()
                .map(|engine| engine.model().draft().revision),
            binding: ProfileBinding::Serial,
            intent: capture_catalog::ModeIntent::RestoreProfile,
            origin: SelectionOrigin::User { new_source: false },
        });
        physical
    }

    #[test]
    fn source_mode_completion_commits_once_and_rejected_busy_input_never_replays() {
        let (mut runtime, drivers) = product_runtime_with_validator(validate_after_admission_probe);
        let validation_guard = ADMISSION_PROBE_VALIDATION.lock();
        let revision = runtime.ui.public_revision;
        let before = runtime.engine.as_ref().unwrap().model().draft().clone();
        let physical = pending_source_fixture(&mut runtime, 71);
        runtime.set_interpolation(&revision.to_string(), true);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "busy");
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &before);
        assert_eq!(runtime.ui.public_revision, revision);
        runtime
            .ui
            .catalog
            .complete_mode_fixture(capture_catalog::ModeReply {
                serial: 70,
                revision: runtime.ui.catalog.revision,
                physical: physical.clone(),
                result: Ok(settings().video.mode),
            });
        runtime.poll();
        assert!(runtime.ui.resolving.is_some());
        assert_eq!(runtime.ui.public_revision, revision);
        runtime
            .ui
            .catalog
            .complete_mode_fixture(capture_catalog::ModeReply {
                serial: 71,
                revision: runtime.ui.catalog.revision,
                physical: physical.clone(),
                result: Ok(settings().video.mode),
            });
        runtime.poll();
        assert!(runtime.ui.resolving.is_none());
        assert_eq!(runtime.ui.public_revision, revision + 1);
        assert!(
            !runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .renderer
                .values()
                .interpolation
        );
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "busy");
        let submitted = runtime.ui.submitted.clone();
        runtime
            .ui
            .catalog
            .complete_mode_fixture(capture_catalog::ModeReply {
                serial: 71,
                revision: runtime.ui.catalog.revision,
                physical,
                result: Ok(settings().video.mode),
            });
        runtime.poll();
        assert_eq!(runtime.ui.public_revision, revision + 1);
        assert_eq!(runtime.ui.submitted, submitted);
        runtime.quit();
        drop(validation_guard);
        cleanup(&mut runtime);
        assert!(drivers.try_recv().is_err());
    }

    #[test]
    fn stale_source_resolution_catalog_domain_and_shutdown_never_install_candidate() {
        for stale in 0..6 {
            let (mut runtime, drivers) = product_runtime();
            let before = runtime.engine.as_ref().unwrap().model().draft().clone();
            let revision = runtime.ui.public_revision;
            let selected = runtime.ui.selected.clone();
            let physical = pending_source_fixture(&mut runtime, 19);
            let catalog = runtime.ui.catalog.revision;
            match stale {
                0 => runtime.ui.catalog.revision += 1,
                1 => {
                    let engine = runtime.engine.as_mut().unwrap();
                    let mut settings = engine.model().draft().settings.clone();
                    settings.renderer = renderer_settings_with_interpolation(true);
                    engine
                        .edit_draft(engine.model().draft().revision, settings)
                        .unwrap();
                }
                2 => {
                    runtime.request_application_close();
                }
                3 => {
                    runtime.request_reset();
                }
                4 => {
                    runtime.ui.catalog = capture_catalog::Model::fixture(
                        crate::capture::linux::empty_snapshot(),
                        Vec::new(),
                    );
                }
                _ => {
                    runtime.ui.catalog.audio_fresh = false;
                    runtime.ui.catalog.revision += 1;
                }
            }
            runtime
                .ui
                .catalog
                .complete_mode_fixture(capture_catalog::ModeReply {
                    serial: 19,
                    revision: catalog,
                    physical,
                    result: Ok(CaptureModeRequest {
                        size: FrameSize::new(1920, 1080).unwrap(),
                        rate: FrameRate::new(30, 1).unwrap(),
                        format_policy: InputFormatPolicy::Auto,
                    }),
                });
            runtime.poll();
            assert!(runtime.ui.resolving.is_none());
            assert_eq!(runtime.ui.selected, selected);
            assert_eq!(
                runtime
                    .engine
                    .as_ref()
                    .unwrap()
                    .model()
                    .draft()
                    .settings
                    .video,
                before.settings.video
            );
            assert!(runtime.ui.submitted.is_none());
            assert_eq!(runtime.ui.public_revision, revision + u64::from(stale == 1));
            assert!(runtime.ui.error.is_some() || stale == 3);
            cleanup(&mut runtime);
            assert!(drivers.try_recv().is_err());
        }
    }

    #[test]
    fn pause_pending_refuses_selection_before_audio_provenance_or_revisions_change() {
        let (mut runtime, drivers) = product_runtime();
        let driver = start_live(&mut runtime, &drivers);
        runtime.ui.audio_policy = crate::app::settings::StartupAudioPolicy::AutoCard;
        let attempt = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .active()
            .unwrap()
            .attempt();
        assert_eq!(runtime.pause(attempt), SubmitStatus::Accepted);
        let before = runtime.engine.as_ref().unwrap().model().draft().clone();
        let revision = runtime.ui.public_revision;
        runtime.set_audio_enabled(&revision.to_string(), false);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "busy");
        assert_eq!(
            runtime.ui.audio_policy,
            crate::app::settings::StartupAudioPolicy::AutoCard
        );
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &before);
        assert_eq!(runtime.ui.public_revision, revision);
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }

    fn card_audio_catalog_fixture(runtime: &mut RuntimeCoordinator) -> AudioSourceIdentity {
        let physical = runtime.ui.catalog.sources[0].physical();
        let identity = AudioSourceIdentity::new(
            "card.capture".into(),
            vec![("device.serial".into(), "GENKI_ShadowCast_3_fixture".into())],
        )
        .unwrap();
        runtime.ui.catalog = capture_catalog::Model::fixture_with_audio(
            session_fixture(&["/dev/video0"], settings().video.fixture_mode()),
            vec![crate::capture::audio::CatalogSource {
                source: crate::capture::audio::AudioSource {
                    identity: identity.clone(),
                    description: "Capture card".into(),
                },
                usb: Ok(Some(crate::capture::audio::UsbAssociation {
                    syspath: physical.usb,
                    identity: physical.identity,
                })),
            }],
        );
        product_values(runtime);
        identity
    }

    #[test]
    fn automatic_card_audio_is_one_source_transaction_without_saving_candidate() {
        let (mut runtime, _) = product_runtime();
        let store = WritableSettings::new();
        store.bind(&mut runtime);
        let identity = card_audio_catalog_fixture(&mut runtime);
        runtime.ui.audio_policy = crate::app::settings::StartupAudioPolicy::AutoCard;
        let revision = runtime.ui.public_revision;
        let source = runtime.ui.catalog.projection.source_rows[0].key.clone();
        runtime.select_capture(
            &revision.to_string(),
            &runtime.ui.catalog.revision.to_string(),
            &source,
        );
        let requested = &runtime.engine.as_ref().unwrap().model().draft().settings;
        assert_eq!(
            requested.audio,
            AudioSelection::Enabled { source: identity }
        );
        assert_eq!(runtime.ui.public_revision, revision + 1);
        assert!(runtime.ui.submitted.is_some());
        assert_eq!(
            runtime.persistence.preferences().gain,
            PlaybackGain::new(100, false).unwrap()
        );
        assert_eq!(
            runtime.persistence.preferences().output,
            PersistentOutputChoice::Auto
        );
        assert!(
            !store.path().exists(),
            "an accepted candidate is not a verified receipt"
        );
        cleanup(&mut runtime);
    }

    #[test]
    fn explicit_saved_audio_off_and_zero_muted_gain_survive_source_choice() {
        let (mut runtime, _) = product_runtime();
        let store = WritableSettings::new();
        runtime.persistence = PersistenceSession::load(Ok(store.path()));
        remember_profile(&mut runtime.persistence, settings());
        card_audio_catalog_fixture(&mut runtime);
        runtime.ui.audio_policy = crate::app::settings::StartupAudioPolicy::Explicit;
        assert_eq!(runtime.set_volume(0), SubmitStatus::Accepted);
        assert_eq!(runtime.set_muted(true), SubmitStatus::Accepted);
        let source = runtime.ui.catalog.projection.source_rows[0].key.clone();
        runtime.select_capture(
            &runtime.ui.public_revision.to_string(),
            &runtime.ui.catalog.revision.to_string(),
            &source,
        );
        assert!(
            !runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .audio
                .enabled()
        );
        assert_eq!(
            runtime.ui.audio_policy,
            crate::app::settings::StartupAudioPolicy::Explicit
        );
        assert_eq!(
            runtime.persistence.preferences().gain,
            PlaybackGain::new(0, true).unwrap()
        );
        assert_eq!(
            runtime.engine.as_ref().unwrap().gain(),
            PlaybackGain::new(0, true).unwrap()
        );
        cleanup(&mut runtime);
    }

    #[test]
    fn retiring_owner_refuses_selection_before_candidate_and_revision_mutation() {
        let (mut runtime, drivers) = product_runtime();
        let driver = start_live(&mut runtime, &drivers);
        let attempt = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .active()
            .unwrap()
            .attempt();
        let closing = runtime.close(attempt.get());
        let before = runtime.engine.as_ref().unwrap().model().draft().clone();
        let revision = runtime.ui.public_revision;
        let selected = runtime.ui.selected.clone();
        let refused = runtime.set_interpolation(&revision.to_string(), true);
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "busy");
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &before);
        assert_eq!(runtime.ui.public_revision, revision);
        assert_eq!(runtime.ui.selected, selected);
        let retired = if closing.release_native {
            closing
        } else if refused.release_native {
            refused
        } else {
            await_update(&mut runtime, |update| update.release_native)
        };
        driver.destroyed.recv().unwrap();
        runtime.native_released(AttemptId::new(retired.generation).unwrap());
        cleanup(&mut runtime);
        assert!(drivers.try_recv().is_err());
    }

    #[test]
    fn explicit_video_field_edit_keeps_the_other_requested_mode_components() {
        for rate_edit in [true, false] {
            let (mut runtime, _) = product_runtime();
            let before = runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .video
                .mode;
            let revision = runtime.ui.public_revision;
            let catalog = runtime.ui.catalog.revision.to_string();
            if rate_edit {
                runtime.set_rate(&revision.to_string(), &catalog, "60000/1001");
            } else {
                runtime.set_size(&revision.to_string(), &catalog, "1920", "1080");
            }
            let requested = runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .video
                .mode;
            assert_eq!(requested.format_policy, before.format_policy);
            if rate_edit {
                assert_eq!(requested.size, before.size);
                assert_eq!(requested.rate, FrameRate::new(60000, 1001).unwrap());
            } else {
                assert_eq!(requested.rate, before.rate);
                assert_eq!(requested.size, FrameSize::new(1920, 1080).unwrap());
            }
            assert_eq!(runtime.ui.public_revision, revision + 1);
            assert!(runtime.ui.submitted.is_some());
            cleanup(&mut runtime);
        }
    }
    fn profile_fixture_settings(index: u8) -> DraftSettings {
        use crate::domain::capture::{DeviceIdentity, UsbTopology};
        let mut value = settings();
        value.video.identity = DeviceIdentity::new(
            0x1234,
            0xabcd,
            UsbTopology::new(
                "pci-profiles".into(),
                vec![std::num::NonZeroU8::new(index).unwrap()],
            )
            .unwrap(),
            Some(format!("profile-{index}")),
        )
        .unwrap();
        value.video.mode.size = if index == 2 {
            FrameSize::new(1920, 1080).unwrap()
        } else {
            FrameSize::new(1280, 720).unwrap()
        };
        value.video.mode.rate = if index == 1 {
            FrameRate::new(60000, 1001).unwrap()
        } else {
            FrameRate::new(30, 1).unwrap()
        };
        let fourcc = CapturedFourCc::from_bytes(if index == 2 { *b"YUYV" } else { *b"NV12" });
        value.video.mode.format_policy = InputFormatPolicy::Manual { fourcc };
        value.video.resolved_fourcc = Some(fourcc);
        value.audio = AudioSelection::Disabled {
            retained: Some(
                AudioSourceIdentity::new(format!("saved-audio-{index}"), Vec::new()).unwrap(),
            ),
        };
        value.filters = treatment_chain(index == 2);
        value.renderer = renderer_settings_with_interpolation(index == 1);
        value
    }
    fn profiles_capture_fixture() -> crate::capture::linux::CaptureSnapshot {
        let a = profile_fixture_settings(1);
        let b = profile_fixture_settings(2);
        let c = profile_fixture_settings(3);
        crate::capture::linux::physical_fixture(&[
            (
                a.video.identity.clone(),
                "/dev/profile-a",
                "/sys/usb/a",
                "/sys/nodes/a",
                a.video.fixture_mode(),
            ),
            (
                b.video.identity.clone(),
                "/dev/profile-b",
                "/sys/usb/b",
                "/sys/nodes/b",
                b.video.fixture_mode(),
            ),
            (
                c.video.identity.clone(),
                "/dev/profile-c",
                "/sys/usb/c",
                "/sys/nodes/c",
                c.video.fixture_mode(),
            ),
        ])
    }
    fn remember_profile(session: &mut PersistenceSession, settings: DraftSettings) {
        use crate::domain::state::{
            ApplyAdmission, ModelEffect, TreatmentAttemptKey, TreatmentPass,
        };
        let mut model = ProductModel::new(settings);
        let submitted = model.draft().clone();
        let (apply, _) = model
            .apply(model.state_identity(), submitted.revision)
            .unwrap();
        let validation = model.validation_request().unwrap().clone();
        let Some(ModelEffect::Open { key, .. }) = model.fixture_validation_succeeded(&validation)
        else {
            panic!("verified profile fixture must open")
        };
        let request = model.opening_request().unwrap();
        let proof = crate::media::controller::test_support::treatment_confirmation(
            TreatmentAttemptKey {
                apply: key.apply,
                attempt: key.attempt,
                pass: TreatmentPass::Open,
            },
            request.playback,
            request.settings.renderer.clone(),
        );
        model.open_verified(key, proof);
        session.admit_user_apply(
            ApplyAdmission::Open { apply },
            submitted,
            ProfileBinding::Serial,
        );
        session.observe_opening(key);
        session.verified_applied(VerifiedApplied::Open {
            key,
            applied: model.last_valid().unwrap().clone(),
        });
    }
    fn validate_profile_fixture_probe(
        request: ValidationRequest,
    ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>) {
        let _guard = PROFILE_VALIDATION_PROBE.lock();
        validate_profile_fixture_inner(request)
    }
    fn validate_profile_fixture_inner(
        request: ValidationRequest,
    ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>) {
        let result = crate::capture::validate_prepared(
            request.settings.clone(),
            &profiles_capture_fixture(),
            &profile_audio_sources(),
            request.watch,
        );
        (request, result)
    }

    fn profile_audio_sources() -> Vec<crate::capture::audio::AudioSource> {
        ["saved-audio-1", "cli-source-b"]
            .into_iter()
            .map(|name| crate::capture::audio::AudioSource {
                identity: AudioSourceIdentity::new(name.into(), Vec::new()).unwrap(),
                description: name.into(),
            })
            .collect()
    }
    static PROFILE_VALIDATION_PROBE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    fn profiles_runtime(store: &WritableSettings) -> (RuntimeCoordinator, mpsc::Receiver<Driver>) {
        let (mut runtime, _) = product_runtime();
        let (tx, rx) = mpsc::channel();
        let runner = GateRunner::with_spawner(
            fixture_treatment_capabilities(),
            move |generation, config| {
                let requested = config
                    .video
                    .validate_snapshot(&profiles_capture_fixture())
                    .unwrap()
                    .requested()
                    .clone();
                let (driver, backend) = Driver::pair(Config::default());
                let backend = backend.with_open_treatments(config.treatment_key, config.treatments);
                tx.send(driver).unwrap();
                OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || backend)
            },
        );
        runtime.persistence = PersistenceSession::load(Ok(store.path()));
        runtime.output = OutputPolicy::new(runtime.persistence.preferences().output.clone());
        runtime.engine = Some(ApplyCoordinator::new(
            settings(),
            runtime.persistence.preferences().gain,
            CaptureValidator::with_runner_and_snapshot(
                validate_profile_fixture_probe,
                profiles_capture_fixture(),
                profile_audio_sources(),
            ),
            runner,
        ));
        let c_audio = AudioSourceIdentity::new("fresh-c-card".into(), Vec::new()).unwrap();
        let c_identity = profile_fixture_settings(3).video.identity;
        let mut audio = profile_audio_sources()
            .into_iter()
            .map(|source| crate::capture::audio::CatalogSource {
                source,
                usb: Ok(None),
            })
            .collect::<Vec<_>>();
        audio.push(crate::capture::audio::CatalogSource {
            source: crate::capture::audio::AudioSource {
                identity: c_audio,
                description: "Fresh card C".into(),
            },
            usb: Ok(Some(crate::capture::audio::UsbAssociation {
                syspath: "/sys/usb/c".into(),
                identity: c_identity,
            })),
        });
        runtime.ui.catalog =
            capture_catalog::Model::fixture_with_audio(profiles_capture_fixture(), audio);
        runtime.ui.selected = None;
        runtime.ui.selected_binding = None;
        runtime.ui.domain_revision = None;
        runtime.ui.state_initialized = false;
        runtime.poll();
        (runtime, rx)
    }
    fn choose_profile_source(runtime: &mut RuntimeCoordinator, index: usize) -> UiUpdate {
        product_values(runtime);
        let key = runtime.ui.catalog.projection.source_rows[index].key.clone();
        runtime.select_capture(
            &runtime.ui.public_revision.to_string(),
            &runtime.ui.catalog.revision.to_string(),
            &key,
        )
    }
    fn complete_profile_source(
        runtime: &mut RuntimeCoordinator,
        drivers: &mpsc::Receiver<Driver>,
        mut update: UiUpdate,
        old: Option<Driver>,
    ) -> Driver {
        if let Some(old) = old {
            if !update.release_native {
                update = await_update(runtime, |update| update.release_native);
            }
            old.destroyed.recv().unwrap();
            update = runtime.native_released(AttemptId::new(update.generation).unwrap());
        }
        if !update.create_native {
            update = await_update(runtime, |update| update.create_native);
        }
        let generation = Generation::new(update.generation).unwrap();
        let driver = drivers.recv().unwrap();
        runtime.surface_ready(SurfaceToken {
            generation,
            xid: X11WindowId::new(71).unwrap(),
        });
        driver.initialized.recv().unwrap();
        let (load, _) = driver.submitted.recv().unwrap();
        driver.send(BackendEvent::CommandReply {
            id: load.get(),
            error: 0,
        });
        let audio = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .settings
            .audio
            .clone();
        if let AudioSelection::Enabled { source } = audio {
            let engine = runtime.engine.as_mut().unwrap();
            let watch = engine.model().observation().unwrap().stamp;
            let attempt = engine.model().state_identity().attempt().unwrap();
            let epoch = AudioEpoch::new(1).unwrap();
            let revision = engine.output_plan().revision().next();
            let target = crate::domain::output::LiveSinkTarget::new(
                SinkIdentity::new("profile-test-output".into(), Vec::new()).unwrap(),
                std::num::NonZeroU64::new(4242).unwrap(),
                17,
            )
            .unwrap();
            engine
                .set_output_plan(crate::domain::output::OutputPlan::Target {
                    revision,
                    target: target.clone(),
                })
                .unwrap();
            let (_, command) = driver
                .submitted
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("accepted output plan reaches owner");
            assert!(matches!(
                command,
                crate::media::controller::BackendCommand::SetOutput(_)
            ));
            driver.send(BackendEvent::AudioAvailability(
                AudioAvailability::Opening { epoch },
            ));
            driver.send(BackendEvent::AudioAvailability(AudioAvailability::Active {
                route: crate::media::loopback::LoopbackReceipt::for_test(
                    generation, attempt, epoch, watch, source, target, revision,
                ),
            }));
        }
        driver.send(BackendEvent::FileLoaded);
        driver.send(BackendEvent::PlaybackRestart);
        driver.confirm_open_treatments();
        driver.fence();
        await_update(runtime, |update| update.product_phase == "Active");
        driver
    }

    #[test]
    fn source_profiles_a_b_a_restore_six_fields_atomically_and_new_c_is_independent() {
        let store = WritableSettings::new();
        let a = profile_fixture_settings(1);
        let b = profile_fixture_settings(2);
        let mut session = PersistenceSession::load(Ok(store.path()));
        let globals = crate::settings::LocalPreferences {
            gain: PlaybackGain::new(0, true).unwrap(),
            fullscreen: true,
            stats_hidden: true,
            output: PersistentOutputChoice::Manual(
                SinkIdentity::new("global-output".into(), Vec::new()).unwrap(),
            ),
        };
        session.set_preferences(globals.clone());
        remember_profile(&mut session, a.clone());
        remember_profile(&mut session, b.clone());
        let (mut runtime, drivers) = profiles_runtime(&store);
        let mut incumbent = None;
        for (index, expected) in [(0, &a), (1, &b), (0, &a)] {
            let revision = runtime.ui.public_revision;
            let update = choose_profile_source(&mut runtime, index);
            let submitted = &runtime.engine.as_ref().unwrap().model().draft().settings;
            assert!(submitted.same_request(expected));
            assert_eq!(runtime.ui.public_revision, revision + 1);
            assert!(runtime.persistence.authorized_apply().is_some());
            assert_eq!(runtime.persistence.preferences(), &globals);
            assert_eq!(runtime.output.choice(), &globals.output);
            assert_eq!(runtime.engine.as_ref().unwrap().gain(), globals.gain);
            incumbent = Some(complete_profile_source(
                &mut runtime,
                &drivers,
                update,
                incumbent,
            ));
            assert!(store.applied().same_request(expected));
            assert_eq!(runtime.persistence.preferences(), &globals);
        }
        let c_update = choose_profile_source(&mut runtime, 2);
        let selected_c = &runtime.engine.as_ref().unwrap().model().draft().settings;
        assert_eq!(
            selected_c.video.identity,
            profile_fixture_settings(3).video.identity
        );
        assert_eq!(selected_c.video.mode.format_policy, InputFormatPolicy::Auto);
        assert!(selected_c.filters.entries().is_empty());
        assert_eq!(selected_c.renderer, RendererSettings::default());
        assert_eq!(
            selected_c.audio,
            AudioSelection::Enabled {
                source: AudioSourceIdentity::new("fresh-c-card".into(), Vec::new()).unwrap(),
            }
        );
        assert_eq!(runtime.persistence.preferences(), &globals);
        assert!(
            runtime
                .persistence
                .source_profile(&selected_c.video.identity, true)
                .is_none()
        );
        // C stays an admitted candidate: the probe holds its validation until
        // shutdown, so no owner opens and no profile can be written. Release
        // the probe only after the close has cancelled the pending validation.
        let validation_probe = PROFILE_VALIDATION_PROBE.lock();
        runtime.request_application_close();
        if c_update.release_native {
            incumbent.take().unwrap().destroyed.recv().unwrap();
            runtime.native_released(AttemptId::new(c_update.generation).unwrap());
        }
        drop(validation_probe);
        cleanup(&mut runtime);
        let restored = PersistenceSession::load(Ok(store.path()));
        assert_eq!(restored.source_profile(&a.video.identity, true), Some(&a));
        assert_eq!(restored.source_profile(&b.video.identity, true), Some(&b));
        assert!(
            restored
                .source_profile(&profile_fixture_settings(3).video.identity, true)
                .is_none()
        );
    }

    #[test]
    fn cli_audio_override_survives_profile_switch_until_explicit_audio_action() {
        let store = WritableSettings::new();
        let mut session = PersistenceSession::load(Ok(store.path()));
        remember_profile(&mut session, profile_fixture_settings(1));
        let (mut runtime, drivers) = profiles_runtime(&store);
        let cli = AudioSelection::Disabled {
            retained: Some(AudioSourceIdentity::new("cli-retained".into(), Vec::new()).unwrap()),
        };
        runtime.ui.cli_audio = Some(cli.clone());
        runtime.ui.audio_policy = crate::app::settings::StartupAudioPolicy::Cli;
        let update = choose_profile_source(&mut runtime, 0);
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .audio,
            cli
        );
        let driver = complete_profile_source(&mut runtime, &drivers, update, None);
        runtime.set_audio_enabled(&runtime.ui.public_revision.to_string(), false);
        assert!(runtime.ui.cli_audio.is_none());
        assert_eq!(
            runtime.ui.audio_policy,
            crate::app::settings::StartupAudioPolicy::Explicit
        );
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn adapted_startup_uses_profile_worker_and_saves_only_after_real_open_proof() {
        let store = WritableSettings::new();
        let expected = profile_fixture_settings(1);
        let mut stale = expected.clone();
        stale.video.mode.size = FrameSize::new(640, 480).unwrap();
        stale.video.mode.rate = FrameRate::new(120, 1).unwrap();
        let mut session = PersistenceSession::load(Ok(store.path()));
        remember_profile(&mut session, stale.clone());
        let before = std::fs::read(store.path()).unwrap();
        let (mut runtime, drivers) = profiles_runtime(&store);
        let engine = runtime.engine.as_mut().unwrap();
        engine
            .edit_draft(engine.model().draft().revision, stale)
            .unwrap();
        runtime.sync_ui_tokens();
        runtime.ui.startup_adaptation = true;
        let opening = runtime.poll();
        assert!(runtime.startup_apply.is_some());
        assert!(runtime.persistence.authorized_apply().is_some());
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .video
                .mode,
            expected.video.mode
        );
        let driver = complete_profile_source(&mut runtime, &drivers, opening, None);
        assert!(runtime.startup_apply.is_none());
        assert!(runtime.persistence.authorized_apply().is_none());
        assert!(store.applied().same_request(&expected));
        assert!(runtime.persistence.startup_reason().contains("verified"));
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn adapted_startup_cli_audio_stays_session_only_while_verified_video_is_saved() {
        for cli_source in [false, true] {
            for originally_enabled in [false, true] {
                let store = WritableSettings::new();
                let mut expected = profile_fixture_settings(1);
                if originally_enabled {
                    expected.audio = AudioSelection::Enabled {
                        source: expected.audio.source().unwrap().clone(),
                    };
                }
                let mut stale = expected.clone();
                stale.video.mode.size = FrameSize::new(640, 480).unwrap();
                stale.video.mode.rate = FrameRate::new(120, 1).unwrap();
                let mut session = PersistenceSession::load(Ok(store.path()));
                remember_profile(&mut session, stale.clone());
                let globals = session.preferences().clone();
                let before = std::fs::read(store.path()).unwrap();
                let (mut runtime, drivers) = profiles_runtime(&store);
                let cli = if cli_source {
                    AudioSelection::Enabled {
                        source: AudioSourceIdentity::new("cli-source-b".into(), Vec::new())
                            .unwrap(),
                    }
                } else {
                    AudioSelection::Disabled {
                        retained: stale.audio.source().cloned(),
                    }
                };
                stale.audio = cli.clone();
                let engine = runtime.engine.as_mut().unwrap();
                engine
                    .edit_draft(engine.model().draft().revision, stale)
                    .unwrap();
                runtime.ui.cli_audio = Some(cli.clone());
                runtime.ui.audio_policy = crate::app::settings::StartupAudioPolicy::Cli;
                runtime.sync_ui_tokens();
                runtime.ui.startup_adaptation = true;
                let opening = runtime.poll();
                assert!(
                    runtime.persistence.authorized_apply().is_some(),
                    "cli_source={cli_source}, originally_enabled={originally_enabled}, startup={}, ui_error={:?}, command_error={}, saved={:?}, binding={:?}, startup_apply={:?}, state={:?}",
                    runtime.persistence.startup_reason(),
                    runtime.ui.error,
                    runtime.command_error,
                    runtime.persistence.saved_selection(),
                    runtime.persistence.saved_binding(),
                    runtime.startup_apply,
                    runtime.engine.as_ref().unwrap().model().state_identity(),
                );
                assert_eq!(std::fs::read(store.path()).unwrap(), before);
                assert_eq!(
                    runtime
                        .engine
                        .as_ref()
                        .unwrap()
                        .model()
                        .draft()
                        .settings
                        .audio,
                    cli
                );
                let mut driver = complete_profile_source(&mut runtime, &drivers, opening, None);
                let applied = runtime
                    .engine
                    .as_ref()
                    .unwrap()
                    .model()
                    .last_valid()
                    .unwrap();
                assert_eq!(applied.settings().audio, cli);
                assert_eq!(applied.settings().video.mode, expected.video.mode);
                assert!(store.applied().same_request(&expected));
                let restored = PersistenceSession::load(Ok(store.path()));
                assert_eq!(restored.saved_selection(), Some(expected.clone()));
                assert_eq!(restored.preferences(), &globals);
                if cli_source {
                    let update =
                        runtime.set_audio_enabled(&runtime.ui.public_revision.to_string(), false);
                    assert!(runtime.ui.cli_audio.is_none());
                    driver = complete_profile_source(&mut runtime, &drivers, update, Some(driver));
                    expected.audio = AudioSelection::Disabled {
                        retained: cli.source().cloned(),
                    };
                    assert!(store.applied().same_request(&expected));
                }
                cleanup(&mut runtime);
                driver.destroyed.recv().unwrap();
            }
        }
    }

    #[test]
    fn unchanged_startup_cli_audio_does_not_authorize_profile_overwrite() {
        let store = WritableSettings::new();
        let saved = profile_fixture_settings(1);
        let mut session = PersistenceSession::load(Ok(store.path()));
        remember_profile(&mut session, saved.clone());
        let before = std::fs::read(store.path()).unwrap();
        let (mut runtime, drivers) = profiles_runtime(&store);
        let cli = AudioSelection::Disabled { retained: None };
        let mut initial = saved.clone();
        initial.audio = cli.clone();
        let engine = runtime.engine.as_mut().unwrap();
        engine
            .edit_draft(engine.model().draft().revision, initial)
            .unwrap();
        runtime.ui.cli_audio = Some(cli.clone());
        runtime.ui.audio_policy = crate::app::settings::StartupAudioPolicy::Cli;
        runtime.sync_ui_tokens();
        runtime.ui.startup_adaptation = true;
        let opening = runtime.poll();
        assert!(runtime.persistence.authorized_apply().is_none());
        let driver = complete_profile_source(&mut runtime, &drivers, opening, None);
        assert_eq!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .last_valid()
                .unwrap()
                .settings()
                .audio,
            cli
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
        assert_eq!(
            PersistenceSession::load(Ok(store.path())).saved_selection(),
            Some(saved)
        );
    }

    #[test]
    fn deferred_startup_absent_last_source_or_multiple_nodes_never_picks_another_route() {
        for multiple_nodes in [false, true] {
            let store = WritableSettings::new();
            let saved = profile_fixture_settings(1);
            let mut session = PersistenceSession::load(Ok(store.path()));
            remember_profile(&mut session, saved.clone());
            let before = std::fs::read(store.path()).unwrap();
            let (mut runtime, drivers) = profiles_runtime(&store);
            let engine = runtime.engine.as_mut().unwrap();
            engine
                .edit_draft(engine.model().draft().revision, saved.clone())
                .unwrap();
            runtime.sync_ui_tokens();
            let mode = saved.video.fixture_mode();
            let snapshot = if multiple_nodes {
                crate::capture::linux::physical_fixture(&[
                    (
                        saved.video.identity.clone(),
                        "/dev/profile-a",
                        "/sys/usb/a",
                        "/sys/nodes/a",
                        mode,
                    ),
                    (
                        saved.video.identity.clone(),
                        "/dev/profile-a-2",
                        "/sys/usb/a",
                        "/sys/nodes/a-2",
                        mode,
                    ),
                ])
            } else {
                let b = profile_fixture_settings(2);
                let mode = b.video.fixture_mode();
                crate::capture::linux::physical_fixture(&[(
                    b.video.identity,
                    "/dev/profile-b",
                    "/sys/usb/b",
                    "/sys/nodes/b",
                    mode,
                )])
            };
            runtime.ui.catalog = capture_catalog::Model::fixture(snapshot, Vec::new());
            runtime.ui.startup_adaptation = true;
            runtime.poll();
            assert!(runtime.startup_apply.is_none());
            assert!(runtime.persistence.authorized_apply().is_none());
            assert!(
                runtime
                    .engine
                    .as_ref()
                    .unwrap()
                    .model()
                    .draft()
                    .settings
                    .same_request(&saved)
            );
            assert_eq!(std::fs::read(store.path()).unwrap(), before);
            assert!(drivers.try_recv().is_err());
            cleanup(&mut runtime);
        }
    }
    #[test]
    fn profile_manual_disappearance_applies_auto_with_all_other_fields_and_saves_after_proof() {
        let store = WritableSettings::new();
        let mut old = profile_fixture_settings(1);
        old.video.mode.format_policy = InputFormatPolicy::Manual {
            fourcc: CapturedFourCc::from_bytes(*b"BGR3"),
        };
        old.video.resolved_fourcc = Some(CapturedFourCc::from_bytes(*b"BGR3"));
        let mut session = PersistenceSession::load(Ok(store.path()));
        remember_profile(&mut session, old.clone());
        let before = std::fs::read(store.path()).unwrap();
        let (mut runtime, drivers) = profiles_runtime(&store);
        let opening = choose_profile_source(&mut runtime, 0);
        let submitted = &runtime.engine.as_ref().unwrap().model().draft().settings;
        assert_eq!(submitted.video.mode.format_policy, InputFormatPolicy::Auto);
        assert_eq!(submitted.filters, old.filters);
        assert_eq!(submitted.renderer, old.renderer);
        assert_eq!(submitted.audio, old.audio);
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        let driver = complete_profile_source(&mut runtime, &drivers, opening, None);
        let saved = store.applied();
        assert_eq!(saved.video.mode.format_policy, InputFormatPolicy::Auto);
        assert_eq!(saved.filters, old.filters);
        assert_eq!(saved.renderer, old.renderer);
        assert_eq!(saved.audio, old.audio);
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn unavailable_profile_audio_is_not_replaced_and_failed_restore_preserves_known_profile() {
        let store = WritableSettings::new();
        let mut known = profile_fixture_settings(1);
        known.audio = AudioSelection::Enabled {
            source: AudioSourceIdentity::new("missing-saved-input".into(), Vec::new()).unwrap(),
        };
        let mut session = PersistenceSession::load(Ok(store.path()));
        remember_profile(&mut session, known.clone());
        let before = std::fs::read(store.path()).unwrap();
        let (mut runtime, drivers) = profiles_runtime(&store);
        let update = choose_profile_source(&mut runtime, 0);
        // Spec: session-only FourCC resolution is cleared before re-admission;
        // only the requested policy is authoritative.
        let mut seeded = known.clone();
        seeded.video.resolved_fourcc = None;
        assert_eq!(
            runtime.engine.as_ref().unwrap().model().draft().settings,
            seeded
        );
        assert_eq!(
            runtime.ui.error.as_ref().map(|error| error.code),
            Some("audio_error"),
            "missing saved audio refuses before any owner admission"
        );
        assert_eq!(runtime.ui.submitted, None);
        assert!(drivers.try_recv().is_err());
        // The refused candidate stays editable: an explicit audio repair is the
        // only authority that revokes the restored-audio intent.
        let revision = runtime.ui.public_revision.to_string();
        runtime.set_audio_enabled(&revision, false);
        assert!(runtime.ui.error.is_none());
        assert!(
            !runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .audio
                .enabled()
        );
        assert!(!runtime.ui.profile_audio);
        // New owner identity after the rejected candidate: no stale old driver.
        let driver = complete_profile_source(&mut runtime, &drivers, update, None);
        let repaired = store.applied();
        // The verified repair saves the corrected selection: same six fields
        // with the explicit Disabled{retained} input, not the refused enabled
        // profile. `same_request` ignores the cleared session-only FourCC.
        let mut expected = known.clone();
        expected.audio = AudioSelection::Disabled {
            retained: known.audio.source().cloned(),
        };
        assert!(repaired.same_request(&expected));
        assert!(!repaired.audio.enabled());
        assert_ne!(std::fs::read(store.path()).unwrap(), before);
        assert_eq!(
            runtime
                .persistence
                .source_profile(&known.video.identity, true),
            Some(&repaired),
            "the verified corrected apply must upsert the profile"
        );
        cleanup(&mut runtime);
        driver.destroyed.recv().unwrap();
    }
    #[test]
    fn renderer_retry_cannot_bypass_refused_profile_audio_and_diagnostic_moves_to_selected_source()
    {
        let store = WritableSettings::new();
        let mut known = profile_fixture_settings(1);
        known.audio = AudioSelection::Enabled {
            source: AudioSourceIdentity::new("missing-saved-input".into(), Vec::new()).unwrap(),
        };
        // B must also carry a missing ENABLED input so selecting it exercises
        // the same restored-audio refusal, not the healthy Disabled default.
        let mut associated = profile_fixture_settings(2);
        associated.audio = AudioSelection::Enabled {
            source: AudioSourceIdentity::new("missing-input-b".into(), Vec::new()).unwrap(),
        };
        let mut session = PersistenceSession::load(Ok(store.path()));
        remember_profile(&mut session, known.clone());
        remember_profile(&mut session, associated.clone());
        let before = std::fs::read(store.path()).unwrap();
        let (mut runtime, drivers) = profiles_runtime(&store);
        // Spec: session-only FourCC resolution is cleared before re-admission.
        let mut expected_a = known.clone();
        expected_a.video.resolved_fourcc = None;
        let mut expected_b = associated.clone();
        expected_b.video.resolved_fourcc = None;
        // A stays the incumbent with its own AutoCard failure reason.
        let a_update = choose_profile_source(&mut runtime, 0);
        assert_eq!(
            runtime.ui.error.as_ref().map(|error| error.code),
            Some("audio_error")
        );
        assert!(!a_update.release_native);
        // Selecting known B installs its full seed, clears A's association
        // reason, and reports B's own audio refusal.
        choose_profile_source(&mut runtime, 1);
        assert_eq!(
            runtime.engine.as_ref().unwrap().model().draft().settings,
            expected_b
        );
        assert_ne!(
            runtime.ui.audio_reason,
            "Association audio indisponible : indisponible"
        );
        assert_eq!(
            runtime.ui.error.as_ref().map(|error| error.code),
            Some("audio_error")
        );
        assert_eq!(runtime.ui.audio_reason, "");
        // A non-audio edit keeps the refused B candidate editable but never
        // bypasses the restored-audio authority.
        let revision = runtime.ui.public_revision.to_string();
        let catalog = runtime.ui.catalog.revision.to_string();
        runtime.set_rate(&revision, &catalog, "30000/1001");
        let mut retry = expected_b;
        retry.video.mode.rate = FrameRate::new(30000, 1001).unwrap();
        // Compare every per-source request field, not session-only resolution
        // or devnode locators (physical admission is covered by the pin tests).
        assert!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .same_request(&retry)
        );
        assert_eq!(
            runtime.ui.error.as_ref().map(|error| error.code),
            Some("audio_error")
        );
        assert!(drivers.try_recv().is_err());
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        cleanup(&mut runtime);
    }
    #[test]
    fn newer_ui_intent_cancels_deferred_startup_and_late_mode_reply_cannot_overwrite() {
        let (mut runtime, drivers) = product_runtime_with_validator(validate_after_admission_probe);
        let validation_guard = ADMISSION_PROBE_VALIDATION.lock();
        let physical = pending_source_fixture(&mut runtime, 101);
        let original = runtime
            .engine
            .as_ref()
            .unwrap()
            .model()
            .draft()
            .settings
            .video
            .mode;
        runtime.ui.resolving.as_mut().unwrap().origin = SelectionOrigin::Startup { original };
        let catalog = runtime.ui.catalog.revision;
        let revision = runtime.ui.public_revision;
        runtime.set_interpolation(&revision.to_string(), true);
        assert!(runtime.ui.resolving.is_none());
        assert!(
            runtime
                .engine
                .as_ref()
                .unwrap()
                .model()
                .draft()
                .settings
                .renderer
                .values()
                .interpolation
        );
        let newer = runtime.engine.as_ref().unwrap().model().draft().clone();
        runtime
            .ui
            .catalog
            .complete_mode_fixture(capture_catalog::ModeReply {
                serial: 101,
                revision: catalog,
                physical,
                result: Ok(CaptureModeRequest {
                    format_policy: InputFormatPolicy::Auto,
                    size: FrameSize::new(1920, 1080).unwrap(),
                    rate: FrameRate::new(30, 1).unwrap(),
                }),
            });
        runtime.poll();
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &newer);
        assert!(runtime.startup_apply.is_none());
        runtime.quit();
        drop(validation_guard);
        cleanup(&mut runtime);
        assert!(drivers.try_recv().is_err());
    }

    #[test]
    fn mode_completion_that_becomes_busy_cannot_mutate_selection_or_profiles() {
        let (mut runtime, drivers) = product_runtime_with_validator(validate_after_admission_probe);
        let validation_guard = ADMISSION_PROBE_VALIDATION.lock();
        let physical = pending_source_fixture(&mut runtime, 102);
        let before = runtime.engine.as_ref().unwrap().model().draft().clone();
        let selected = runtime.ui.selected.clone();
        let engine = runtime.engine.as_mut().unwrap();
        engine
            .apply(engine.model().state_identity(), before.revision)
            .unwrap();
        runtime
            .ui
            .catalog
            .complete_mode_fixture(capture_catalog::ModeReply {
                serial: 102,
                revision: runtime.ui.catalog.revision,
                physical,
                result: Ok(CaptureModeRequest {
                    format_policy: InputFormatPolicy::Auto,
                    size: FrameSize::new(1920, 1080).unwrap(),
                    rate: FrameRate::new(30, 1).unwrap(),
                }),
            });
        runtime.poll();
        assert_eq!(runtime.engine.as_ref().unwrap().model().draft(), &before);
        assert_eq!(runtime.ui.selected, selected);
        assert!(runtime.ui.submitted.is_none());
        assert!(runtime.persistence.saved_selection().is_none());
        assert_eq!(runtime.ui.error.as_ref().unwrap().code, "busy");
        runtime.quit();
        drop(validation_guard);
        cleanup(&mut runtime);
        assert!(drivers.try_recv().is_err());
    }
}
