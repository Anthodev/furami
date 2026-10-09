//! Qt-thread composition: product coordinator plus capture/media adapters.

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
};

type Engine = ApplyCoordinator<CaptureValidator, GateRunner>;

pub(crate) struct UiUpdate {
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
    pub draft_dirty: bool,
    pub close_dialog: String,
    pub close_revision: u64,
    pub reset_token: u64,
    pub fullscreen: bool,
    pub closing: bool,
    /// Audio output selector projection: pipe-separated `key|label|eligible`
    /// rows (the reserved `auto` row first), the catalog revision the rows
    /// were built from, the user's selected key ("auto" or sink name), the
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

pub(crate) struct RuntimeCoordinator {
    engine: Option<Engine>,
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
    pub(crate) fn new(
        prefix: String,
        filter_capabilities: Result<
            crate::media::filter_catalog::FilterCapabilities,
            crate::media::filter_catalog::FilterCatalogError,
        >,
        startup: StartupSelection,
        persistence: PersistenceSession,
        sources: Vec<AudioSourceIdentity>,
    ) -> Self {
        let gain = persistence.preferences().gain;
        let output = OutputPolicy::new(persistence.preferences().output.clone());
        let (catalog, catalog_start_failed) = match OutputCatalogWatch::start() {
            Ok(watch) => (Some(watch), false),
            Err(error) => {
                tracing::warn!(error = %error, "output catalog watch unavailable");
                (None, true)
            }
        };
        Self {
            engine: startup.draft.map(|settings| {
                ApplyCoordinator::new(
                    settings,
                    gain,
                    CaptureValidator::new(),
                    GateRunner::new(prefix, filter_capabilities),
                )
            }),
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
    pub(crate) fn capture_selected(&self) -> bool {
        self.engine.is_some()
    }
    pub(crate) fn ui_ready(&mut self) -> UiUpdate {
        if std::mem::take(&mut self.auto_open)
            && self.persistence.mutations_open()
            && let Some(engine) = &mut self.engine
        {
            match engine.apply(
                engine.model().state_identity(),
                engine.model().draft().revision,
            ) {
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
        // The catalog worker must stop before the Qt loop exits: request the
        // stop here, then let poll() drain its non-blocking try_join until
        // the worker is actually complete. Drop only guarantees the final
        // join fallback after Qt exit, never the quit barrier itself.
        self.catalog_stopping = true;
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
        let draft = self.engine.as_ref().map(|engine| {
            (
                &engine.model().draft().settings,
                engine.model().draft().revision,
            )
        });
        if self.persistence.request_close(draft) {
            self.begin_shutdown();
        }
        self.dirty = true;
        self.poll()
    }
    pub(crate) fn decide_close(&mut self, discard: bool, revision: u64) -> UiUpdate {
        let draft = self.engine.as_ref().map(|engine| {
            (
                &engine.model().draft().settings,
                engine.model().draft().revision,
            )
        });
        if self
            .persistence
            .decide_dirty_close(discard, DraftRevision::new(revision), draft)
        {
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
        if let Some(engine) = &mut self.engine {
            let submitted = engine.model().draft().clone();
            let result = engine.apply(engine.model().state_identity(), submitted.revision);
            match result {
                Err(error) => self.rejection(error),
                Ok(apply) => {
                    self.persistence.admit_user_apply(apply, submitted);
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

    /// Whole-projection presentation of the selector. Rows are plain
    /// `key|label|eligible` text (the reserved auto row first, then every
    /// catalogued sink) built from the last observed catalog; an absent
    /// catalog still shows the auto row with an empty revision.
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
            label: "Auto (default)".to_owned(),
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
            || identity.filter_pass() != expected.filter_pass
        {
            Err(CommandRejection::StaleState)
        } else {
            Ok(identity)
        }
    }
    pub(crate) fn qualification_command(&mut self, line: &str) -> UiUpdate {
        // Opt-in qualification calls the same production application decisions.
        if !control::is_filter_command(line) {
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
            "close-cancel" => return self.decide_close(false, self.persistence.close_revision()),
            "close-discard" => return self.decide_close(true, self.persistence.close_revision()),
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
                Command::Video(revision, mode) => {
                    let mut settings = engine.model().draft().settings.clone();
                    settings.video.mode = mode;
                    engine
                        .edit_draft(revision, settings)
                        .map_err(|error| error.to_string())?;
                }
                Command::Identity(revision, identity) => {
                    let mut settings = engine.model().draft().settings.clone();
                    settings.video.identity = identity;
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
                    let state =
                        Self::checked_state(engine, expected).map_err(|error| error.to_string())?;
                    let submitted = engine.model().draft().clone();
                    let apply = engine
                        .apply(state, revision)
                        .map_err(|error| error.to_string())?;
                    self.persistence.admit_user_apply(apply, submitted);
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
                    let draft = Some((
                        &engine.model().draft().settings,
                        engine.model().draft().revision,
                    ));
                    if self.persistence.request_close(draft) {
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
                if matches!(&event, VerifiedApplied::Open { key, .. } if self.startup_apply == Some(key.apply))
                {
                    self.startup_apply = None;
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
        let mut update = self.poll_media();
        let base_drained = self.quit_empty
            || self
                .engine
                .as_ref()
                .is_some_and(|engine| engine.model().shutdown_ready());
        let drained = base_drained && self.drain_catalog();
        let previous_dialog = self.persistence.close_dialog();
        if drained {
            self.persistence.drained();
        }
        if previous_dialog != self.persistence.close_dialog() {
            update.changed = true;
        }
        let projection = self.output_projection();
        if self.last_output.as_ref() != Some(&projection) {
            self.last_output = Some(projection.clone());
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
            update.draft_dirty = self.persistence.draft_dirty(
                self.engine
                    .as_ref()
                    .map(|engine| &engine.model().draft().settings),
            );
            update.saved_selection = self
                .engine
                .as_ref()
                .map(|engine| selection_text(&engine.model().draft().settings))
                .unwrap_or_default();
            update.close_dialog = self.persistence.close_dialog().into();
            update.close_revision = self.persistence.close_revision();
            update.reset_token = self.persistence.reset_token();
            update.fullscreen = self.persistence.preferences().fullscreen;
            update.closing = !self.persistence.mutations_open();
        }
        if update.changed {
            tracing::info!(settings_status = %update.settings_status, settings_path = %update.settings_path,
                settings_refused = update.settings_refused, startup_reason = %update.startup_reason,
                draft_dirty = update.draft_dirty, close_dialog = %update.close_dialog, reset_token = update.reset_token,
                fullscreen = update.fullscreen, selection = %update.saved_selection, quit = update.quit, "settings_runtime");
        }
        update
    }
    fn poll_media(&mut self) -> UiUpdate {
        let Some(engine) = &mut self.engine else {
            let changed = std::mem::take(&mut self.dirty);
            return UiUpdate {
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
                        "No capture selected. Start with --capture-node, --capture-fourcc, --capture-size and --capture-rate.".into()
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
                playback_status: "Unavailable".into(),
                create_native: false,
                release_native: false,
                quit: self.quit_empty,
                settings_status: String::new(),
                settings_path: String::new(),
                settings_refused: false,
                saved_selection: String::new(),
                startup_reason: String::new(),
                draft_dirty: false,
                close_dialog: String::new(),
                close_revision: 0,
                reset_token: 0,
                fullscreen: false,
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
        let desired_settings = model
            .active()
            .map(|active| active.applied().settings())
            .or_else(|| model.recovery().map(|recovery| recovery.applied.settings()))
            .or_else(|| model.last_valid().map(|applied| applied.settings()))
            .unwrap_or(&model.draft().settings);
        let desired_source = desired_settings
            .audio
            .source()
            .map(|source| source.name().to_owned())
            .unwrap_or_default();
        let desired_audio_enabled = desired_settings.audio.enabled();
        let can_open =
            self.persistence.mutations_open() && (model.can_apply() || model.can_reconnect());
        let can_restart =
            self.persistence.mutations_open() && (model.can_restart() || model.can_reconnect());
        let restart_generation = identity.attempt().map(AttemptId::get).unwrap_or(0);
        let quit = model.shutdown_ready();
        let mut diagnostic = String::new();
        if changed {
            if let Some(rejection) = model.validation_rejection() {
                diagnostic.push_str(&rejection.failure.to_string());
                if let Some(filter) = &rejection.failure.filter {
                    diagnostic.push_str(&format!(
                        "\nFilter diagnostics: {}",
                        serde_json::json!(filter)
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
                    if let Some(filter) = &failure.filter {
                        diagnostic.push_str(&format!(
                            "\nFilter diagnostics: {}",
                            serde_json::json!(filter)
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
            ProductPhase::ApplyingFilters => "Applying filters",
            ProductPhase::RestoringFilters => "Restoring filters",
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
        let availability = engine.audio_availability().cloned();
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
                .any(|failure| failure.filter.is_some())
        }) || model
            .validation_rejection()
            .is_some_and(|rejection| rejection.failure.filter.is_some());
        if changed
            && diagnostic.is_empty()
            && let Some(fatal) = engine.runner_mut().fatal_native_failure()
        {
            diagnostic = fatal.to_string();
        }
        if changed && diagnostic.is_empty() {
            diagnostic = if engine.runner_mut().report.is_empty() {
                format!(
                    "Requested: {:?}\nAudio requested: {desired_audio_enabled}\nOpen capture to apply draft.",
                    engine.model().draft().settings.video
                )
            } else {
                engine.runner_mut().report.clone()
            };
        }
        if changed {
            log_snapshot(engine);
        }
        let runner = engine.runner_mut();
        UiUpdate {
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
            recovery_evidence,
            recovery_stage,
            candidates,
            paused,
            prepared_paused,
            volume_percent,
            muted,
            can_toggle_pause,
            can_set_gain,
            playback_status: playback_status.into(),
            create_native: native.create_native,
            release_native: native.release_native,
            quit,
            settings_status: String::new(),
            settings_path: String::new(),
            settings_refused: false,
            saved_selection: String::new(),
            startup_reason: String::new(),
            draft_dirty: false,
            close_dialog: String::new(),
            close_revision: 0,
            reset_token: 0,
            fullscreen: false,
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
    format!(
        "USB {:04x}:{:04x}\nController {} / ports {} / serial {}\n{} ({:#010x}) · {}×{} · {}/{} FPS\nAudio {}: {}",
        identity.vendor_id(),
        identity.product_id(),
        identity.topology().controller(),
        ports,
        identity.serial().unwrap_or("none"),
        String::from_utf8_lossy(&mode.captured_fourcc.bytes()),
        mode.captured_fourcc.kernel_value(),
        mode.size.width(),
        mode.size.height(),
        mode.rate.numerator(),
        mode.rate.denominator(),
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

/// Human-readable product phase: the reducer-level truth, projected next to
/// the media gate phase so disconnected/recovering/selection states stay
/// visible even while no media window exists.
fn phase_name(phase: ProductPhase) -> &'static str {
    match phase {
        ProductPhase::Stopped => "Stopped",
        ProductPhase::Active => "Active",
        ProductPhase::ApplyingFilters => "ApplyingFilters",
        ProductPhase::RestoringFilters => "RestoringFilters",
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
    let pending_key = engine
        .model()
        .filtering()
        .map(|transition| transition.key());
    // Admission can precede the owner's first Pending publication. A retained
    // incumbent confirmation is not success for that newly admitted key.
    let filters = match (pending_key, engine.runner_mut().filter_snapshot()) {
        (Some(key), observed)
            if observed.is_none_or(|observed| {
                observed.key != key
                    || matches!(
                        observed.status,
                        crate::media::controller::FilterStatus::Confirmed(_)
                    )
            }) =>
        {
            Some(serde_json::json!({
                "key": key, "sequence": null, "status": "Pending",
                "confirmation": null, "failure": null,
            }))
        }
        (_, Some(filters)) => {
            let (status, confirmation, failure) = match &filters.status {
                crate::media::controller::FilterStatus::Pending => ("Pending", None, None),
                crate::media::controller::FilterStatus::Confirmed(confirmation) => {
                    ("Confirmed", Some(confirmation), None)
                }
                crate::media::controller::FilterStatus::Failed(failure) => {
                    ("Failed", None, Some(failure))
                }
            };
            Some(serde_json::json!({
                "key": filters.key, "sequence": filters.sequence, "status": status,
                "confirmation": confirmation, "failure": failure,
            }))
        }
        _ => None,
    };
    let model = engine.model();
    let identity = model.state_identity();
    serde_json::json!({
        "phase": model.phase(), "apply_id": identity.operation().map(|id| id.get()), "attempt_id": identity.attempt().map(AttemptId::get),
        "filter_pass": identity.filter_pass(), "filters": filters,
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

    fn settings() -> DraftSettings {
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(1280, 720).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        };
        DraftSettings {
            filters: crate::domain::filters::FilterChain::default(),
            video: crate::domain::capture::ModeRequest {
                identity: session_fixture(&["/dev/video0"], mode).devices()[0]
                    .identity()
                    .clone(),
                mode,
            },
            audio: AudioSelection::default(),
        }
    }
    fn validate(
        request: ValidationRequest,
    ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>) {
        let prepared = fixture_prepared(request.settings.clone());
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
        runtime_fixture(execute, None)
    }
    fn runtime_fixture(
        execute: fn(
            ValidationRequest,
        ) -> (ValidationRequest, Result<PreparedCapture, ApplyFailure>),
        fail_spawn: Option<u64>,
    ) -> (RuntimeCoordinator, mpsc::Receiver<Driver>) {
        let (tx, rx) = mpsc::channel();
        let runner = GateRunner::with_spawner(
            Ok(crate::media::filter_catalog::fixture_capabilities()),
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
                let backend = backend.with_open_filters(config.filter_key, config.compiled_filters);
                tx.send(driver).unwrap();
                OwnerEndpoint::spawn_with_backend_requested(generation, requested, move || backend)
            },
        );
        let engine = ApplyCoordinator::new(
            settings(),
            PlaybackGain::default(),
            CaptureValidator::with_runner(execute),
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
                        draft: Some(settings()),
                        auto_open: false,
                        reason: String::new(),
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
        driver.confirm_open_filters();
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
                filter_pass: Some(
                    if expected.filter_pass == Some(crate::domain::state::FilterPass::LiveCandidate)
                    {
                        crate::domain::state::FilterPass::LiveRestore
                    } else {
                        crate::domain::state::FilterPass::LiveCandidate
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
            document
                .applied
                .expect("confirmed user Apply must be saved")
        }
        fn bind(&self, runtime: &mut RuntimeCoordinator) {
            runtime.persistence = PersistenceSession::load(Ok(self.path()));
            runtime.persistence.prepare_startup(&StartupSelection {
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
    fn begin_live_filters(
        runtime: &mut RuntimeCoordinator,
        driver: &Driver,
    ) -> (
        crate::media::controller::RequestId,
        crate::domain::state::FilterAttemptKey,
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
        assert_eq!(pending.product_phase, "ApplyingFilters");
        assert_eq!(pending.playback_status, "Applying filters");
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
        let crate::media::controller::BackendCommand::ApplyFilters { key, .. } = command else {
            panic!("expected exactly one whole-chain filter command: {command:?}")
        };
        assert_eq!(key.pass, crate::domain::state::FilterPass::LiveCandidate);
        assert_eq!(runtime.persistence.authorized_apply(), Some(key.apply));
        (id, key)
    }
    fn confirm_filters(
        driver: &Driver,
        id: crate::media::controller::RequestId,
        key: crate::domain::state::FilterAttemptKey,
    ) {
        driver.send(BackendEvent::FilterResult {
            id: id.get(),
            key,
            result: Ok(
                crate::domain::state::FilterConfirmation::checked(key, 0.0, 2.0, 32).unwrap(),
            ),
        });
        driver.fence();
    }
    fn treatment_failure(
        key: crate::domain::state::FilterAttemptKey,
        kind: crate::domain::failure::FilterErrorKind,
        fresh: bool,
    ) -> Box<crate::domain::failure::FilterFailure> {
        Box::new(crate::domain::failure::FilterFailure {
            kind,
            attributed_ordinal: None,
            requires_fresh_owner: fresh,
            diagnostics: crate::domain::failure::FilterAttemptDiagnostics {
                key: Some(key),
                entries: Vec::new(),
                records: Vec::new(),
                native_evidence_lost: false,
                truncated: false,
                dropped_context: 0,
            },
        })
    }

    #[test]
    fn qualifier_strict_filter_edit_preserves_invalid_draft_and_legacy_boundaries() {
        let (mut runtime, drivers) = runtime();
        let chain = treatment_chain(false);
        let edited = edit_filters(&mut runtime, &chain);
        assert!(edited.draft_dirty && !edited.failed);
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
        exact.push_str(&" ".repeat(control::FILTER_COMMAND_MAX_BYTES - exact.len()));
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
            assert_eq!(initial["filters"]["status"], "Confirmed");
            assert_eq!(initial["filters"]["key"]["pass"], "Open");
            assert_eq!(
                initial["active_settings"]["filters"],
                serde_json::json!(&chain)
            );
            assert!(initial["filter_pass"].is_null());
            let candidate = treatment_chain(!all_disabled);
            edit_filters(&mut runtime, &candidate);
            let (id, key) = begin_live_filters(&mut runtime, &driver);
            let pending = runtime_snapshot(runtime.engine.as_mut().unwrap());
            assert_eq!(pending["phase"], "ApplyingFilters");
            assert_eq!(pending["filter_pass"], "LiveCandidate");
            assert_eq!(pending["filters"]["status"], "Pending");
            assert_eq!(pending["filters"]["key"], serde_json::json!(key));
            assert_eq!(
                pending["active_settings"]["filters"],
                serde_json::json!(&chain)
            );
            edit_filters(
                &mut runtime,
                &crate::domain::filters::FilterChain::default(),
            );
            confirm_filters(&driver, id, key);
            let confirmed = await_update(&mut runtime, |update| update.product_phase == "Active");
            assert!(!confirmed.failed && confirmed.draft_dirty);
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
            assert_eq!(snapshot["filters"]["status"], "Confirmed");
            assert_eq!(snapshot["filters"]["key"]["pass"], "LiveCandidate");
            assert_eq!(
                snapshot["active_settings"]["filters"],
                serde_json::json!(&candidate)
            );
            let bytes = std::fs::read(store.path()).unwrap();
            // Duplicate native outcomes do not regain consumed persistence permission.
            confirm_filters(&driver, id, key);
            runtime.poll();
            assert_eq!(std::fs::read(store.path()).unwrap(), bytes);
            assert_eq!(runtime.persistence.authorized_apply(), None);
            assert!(
                drivers.try_recv().is_err(),
                "filter Apply never reopens the owner"
            );
            cleanup(&mut runtime);
            driver.destroyed.recv().unwrap();
        }
    }

    #[test]
    fn runtime_live_receipt_cannot_borrow_wrong_admission_key_revision_pass_or_submission() {
        use crate::domain::state::{ApplyAdmission, ApplyId, FilterPass};
        for mismatch in 0..6 {
            let store = WritableSettings::new();
            let (mut runtime, drivers) = runtime();
            store.bind(&mut runtime);
            let driver = start_live(&mut runtime, &drivers);
            let original = std::fs::read(store.path()).unwrap();
            edit_filters(&mut runtime, &treatment_chain(false));
            let (id, key) = begin_live_filters(&mut runtime, &driver);
            let mut submitted = runtime.engine.as_ref().unwrap().model().draft().clone();
            let mut wrong = key;
            let mut revision = submitted.revision;
            match mismatch {
                0 => wrong.apply = ApplyId::new(key.apply.get() + 1).unwrap(),
                1 => wrong.attempt = AttemptId::new(key.attempt.get() + 1).unwrap(),
                2 => wrong.pass = FilterPass::LiveRestore,
                3 => revision = DraftRevision::new(revision.get() + 1),
                4 => submitted.revision = DraftRevision::new(revision.get() + 1),
                5 => submitted.settings.filters = crate::domain::filters::FilterChain::default(),
                _ => unreachable!(),
            }
            runtime.persistence.admit_user_apply(
                ApplyAdmission::Filters {
                    key: wrong,
                    revision,
                },
                submitted,
            );
            confirm_filters(&driver, id, key);
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
            let (id, key) = begin_live_filters(&mut runtime, &driver);
            confirm_filters(&driver, id, key);
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
            let (id, key) = begin_live_filters(&mut runtime, &driver);
            confirm_filters(&driver, id, key);
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
                driver.send(BackendEvent::FilterFault {
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
            state::FilterPass,
        };
        for restore_fails in [false, true] {
            let store = WritableSettings::new();
            let (mut runtime, drivers) = runtime();
            store.bind(&mut runtime);
            let driver = start_live(&mut runtime, &drivers);
            let original = std::fs::read(store.path()).unwrap();
            let candidate = treatment_chain(false);
            edit_filters(&mut runtime, &candidate);
            let (id, key) = begin_live_filters(&mut runtime, &driver);
            let stale_close = format!(
                "close ApplyingFilters {} {} Complete",
                key.apply.get(),
                key.attempt.get()
            );
            driver.send(BackendEvent::FilterResult {
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
                update.product_phase == "RestoringFilters"
            });
            assert!(restoring.failed && !restoring.can_open && !restoring.can_toggle_pause);
            assert_eq!(restoring.playback_status, "Restoring filters");
            assert!(
                restoring.diagnostic.contains("Unconfirmed")
                    && restoring.diagnostic.contains("Deadline")
            );
            assert_eq!(runtime.persistence.authorized_apply(), None);
            let (restore_id, command) = driver
                .submitted
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let crate::media::controller::BackendCommand::ApplyFilters { key: restore, .. } =
                command
            else {
                panic!("the selected route must send one live restore command")
            };
            assert_eq!(restore.pass, FilterPass::LiveRestore);
            let snapshot = runtime_snapshot(runtime.engine.as_mut().unwrap());
            assert_eq!(snapshot["filter_pass"], "LiveRestore");
            runtime.qualification_command(&stale_close);
            assert!(runtime.command_error.contains("stale"));
            assert_eq!(
                runtime.engine.as_ref().unwrap().model().phase(),
                ProductPhase::RestoringFilters
            );
            if restore_fails {
                driver.send(BackendEvent::FilterResult {
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
                confirm_filters(&driver, restore_id, restore);
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
        let Some(ModelEffect::Open { key: old, .. }) = model.validation_succeeded(&initial_request)
        else {
            panic!("initial physical opening missing");
        };
        assert!(!prepared_paused_presentation(&model, Some(old.attempt)));
        model.open_verified(old);
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
        let Some(ModelEffect::Open { key: recovered, .. }) = model.validation_succeeded(&request)
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
        model.open_verified(recovered);
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
        model.open_verified(live);
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
        first.confirm_open_filters();
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
        second.confirm_open_filters();
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
        driver.confirm_open_filters();
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
        let before = runtime.engine.as_ref().unwrap().model().state_identity();
        // Replace only the admission port. Keep the real owner alive, then
        // restore its port before cleanup; this is not a physical owner failure.
        let unavailable = GateRunner::with_spawner(
            Ok(crate::media::filter_catalog::fixture_capabilities()),
            |_, _| panic!("no replacement opening requested"),
        );
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
                    let (mut runtime, drivers) = runtime_fixture(validate, restored.then_some(2));
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
                        restored_owner.confirm_open_filters();
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
        let snapshot = session_fixture(&["/dev/video0"], request.settings.video.mode);
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
            Ok(crate::media::filter_catalog::fixture_capabilities()),
            move |generation, config| {
                let requested = config.video.requested();
                let input = config
                    .video
                    .validate_snapshot(&session_fixture(&["/dev/video0"], requested.mode))
                    .unwrap();
                let requested = input.requested().clone();
                let (driver, backend) = Driver::pair(Config::default());
                let backend = backend.with_open_filters(config.filter_key, config.compiled_filters);
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
                    draft: Some(settings()),
                    auto_open: false,
                    reason: String::new(),
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
        driver.confirm_open_filters();
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
        driver.confirm_open_filters();
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
            draft: None,
            auto_open: false,
            reason: "Preferences only".into(),
        };
        let mut runtime = RuntimeCoordinator::new(
            String::new(),
            Ok(crate::media::filter_catalog::fixture_capabilities()),
            startup,
            persistence,
            vec![],
        );
        let initial = runtime.poll();
        assert!(initial.can_set_gain && !initial.can_open && !initial.create_native);
        assert_eq!(runtime.set_volume(27), SubmitStatus::Accepted);
        assert_eq!(runtime.set_muted(true), SubmitStatus::Accepted);
        let update = runtime.set_fullscreen(true);
        assert_eq!(update.volume_percent, 27);
        assert!(update.muted && update.fullscreen && !update.draft_dirty);
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
        let reloaded = PersistenceSession::load(Ok(path));
        assert_eq!(
            reloaded.preferences().gain,
            PlaybackGain::new(27, true).unwrap()
        );
        assert!(reloaded.preferences().fullscreen && reloaded.saved_selection().is_none());
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn dirty_close_cancel_keeps_active_owner_and_discard_drains_before_save_decision() {
        let (mut runtime, drivers) = runtime();
        let driver = start_live(&mut runtime, &drivers);
        runtime.qualification_command("draft-video 0 YUYV 1920x1080 30/1");
        let dirty = runtime.request_application_close();
        assert_eq!(dirty.close_dialog, "draft");
        assert!(dirty.draft_dirty && !dirty.release_native && !dirty.quit);
        assert!(driver.destroyed.try_recv().is_err());
        let cancelled = runtime.decide_close(false, dirty.close_revision);
        assert_eq!(cancelled.close_dialog, "");
        assert!(cancelled.can_toggle_pause && !cancelled.release_native);
        assert!(driver.destroyed.try_recv().is_err());
        runtime.request_application_close();
        let discard = runtime.decide_close(true, dirty.close_revision);
        assert!(!discard.quit);
        let retired = if discard.release_native {
            discard
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
                draft: Some(saved),
                auto_open: true,
                reason: String::new(),
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
            driver.confirm_open_filters();
            driver.fence();
            let restored = await_update(&mut runtime, |update| update.can_toggle_pause);
            assert!(!restored.draft_dirty && !restored.failed);
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
        assert_eq!(rows[0]["label"], "Auto (default)");
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
}
