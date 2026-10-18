//! Correlated, verified user transactions and genuinely adapted startup receipts
//! authorize source-profile updates. Safe close saves the complete working
//! collection and globals, never an uncommitted candidate.
use std::{ffi::OsStr, path::PathBuf};

use crate::{
    app::apply::VerifiedApplied,
    domain::{
        capture::{AudioSelection, AudioSourceIdentity, DeviceIdentity, PlaybackGain},
        state::{ApplyAdmission, ApplyId, AttemptKey, AttemptPurpose, Draft, DraftSettings},
    },
    settings::{
        Durability, LoadOutcome, LocalPreferences, ProfileBinding, SettingsStore, SourceProfiles,
        WriteOutcome,
    },
};

pub fn resolve_settings_path(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf, String> {
    if let Some(value) = xdg.filter(|value| !value.is_empty()) {
        let root = PathBuf::from(value);
        return if root.is_absolute() {
            Ok(root.join("furami/settings.json"))
        } else {
            Err("XDG_CONFIG_HOME must be absolute; settings are unavailable (no relative-file fallback).".into())
        };
    }
    let root = home.filter(|value| !value.is_empty()).map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("Settings are unavailable: HOME must name an absolute directory when XDG_CONFIG_HOME is unset.")?;
    Ok(root.join(".config/furami/settings.json"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupAudioPolicy {
    AutoCard,
    Explicit,
    /// Explicit CLI input audio survives source switches until an audio UI action.
    Cli,
}
#[derive(Debug, PartialEq)]
pub struct StartupSelection {
    pub draft: Option<DraftSettings>,
    pub auto_open: bool,
    pub adapt_video: bool,
    pub restore_profile: bool,
    pub source_binding: Option<ProfileBinding>,
    /// Fresh preflight locators, never persisted and never capture authority.
    pub topology_pin: Option<(PathBuf, PathBuf)>,
    pub reason: String,
    pub audio_policy: StartupAudioPolicy,
    pub bootstrap_audio: crate::domain::capture::AudioSelection,
}

#[derive(Debug)]
pub enum StartupVideoError {
    Incompatible(String),
    Unavailable(String),
}
impl From<String> for StartupVideoError {
    fn from(reason: String) -> Self {
        Self::Unavailable(reason)
    }
}
impl From<&str> for StartupVideoError {
    fn from(reason: &str) -> Self {
        Self::Unavailable(reason.into())
    }
}
impl StartupVideoError {
    pub fn from_failure(error: crate::domain::failure::ApplyFailure) -> Self {
        use crate::domain::failure::{Cause, FailureCategory, ValidationLayer};
        if error.category == FailureCategory::Validation(ValidationLayer::Mode)
            && error.cause == Cause::RequestedModeRefused
        {
            Self::Incompatible(error.to_string())
        } else {
            Self::Unavailable(error.to_string())
        }
    }
}

/// Discovery is not atomic. Normal opening revalidates, and its initial audio
/// receipt is additionally strict. Disabled retained audio never blocks launch.
pub fn decide_startup(
    saved: Option<DraftSettings>,
    explicit: Option<DraftSettings>,
    check_video: impl FnOnce(&DraftSettings) -> Result<(), StartupVideoError>,
    audio: Result<&[AudioSourceIdentity], String>,
) -> StartupSelection {
    if let Some(draft) = explicit {
        return StartupSelection { restore_profile: false, source_binding: None, topology_pin: None, draft: Some(draft), auto_open: false, adapt_video: false, reason: "Explicit command-line selection; saved capture auto-open suspended until a user capture transaction.".into(), audio_policy: StartupAudioPolicy::Explicit, bootstrap_audio: Default::default() };
    }
    let Some(mut draft) = saved else {
        return StartupSelection {
            restore_profile: false,
            source_binding: None,
            topology_pin: None,
            draft: None,
            auto_open: false,
            adapt_video: false,
            audio_policy: StartupAudioPolicy::AutoCard,
            bootstrap_audio: Default::default(),
            reason:
                "No saved capture selection. Local preferences remain available without a stream."
                    .into(),
        };
    };
    if draft.video.mode.format_policy == crate::domain::capture::InputFormatPolicy::Auto {
        draft.video.resolved_fourcc = None;
    }
    let video = check_video(&draft);
    let audio_result = (|| {
        if !draft.audio.enabled() {
            return Ok(());
        }
        let sources = audio?;
        let source = draft
            .audio
            .source()
            .ok_or("Saved enabled audio has no source.")?;
        let mut matches = sources
            .iter()
            .filter(|observed| source.compatible_with(observed));
        match (matches.next(), matches.next()) {
            (Some(_), None) => Ok(()),
            (None, _) => Err(format!(
                "Saved enabled audio source {:?} is absent or its stable identity differs. No stream opened.",
                source.name()
            )),
            _ => Err(format!(
                "Saved enabled audio source {:?} is ambiguous. No stream opened.",
                source.name()
            )),
        }
    })();
    let (adapt_video, result) = match (video, audio_result) {
        (_, Err(reason)) => (false, Err(reason)),
        (Ok(()), Ok(())) => (false, Ok(())),
        (Err(StartupVideoError::Incompatible(reason)), Ok(())) => (true, Err(reason)),
        (Err(StartupVideoError::Unavailable(reason)), Ok(())) => (false, Err(reason)),
    };
    match result {
        Ok(()) => StartupSelection {
            restore_profile: true,
            source_binding: None,
            topology_pin: None,
            draft: Some(draft),
            auto_open: true,
            adapt_video: false,
            audio_policy: StartupAudioPolicy::Explicit,
            bootstrap_audio: Default::default(),
            reason: "Saved sources and exact capture tuple are available; restoring live capture."
                .into(),
        },
        Err(reason) => StartupSelection {
            restore_profile: true,
            source_binding: None,
            topology_pin: None,
            draft: Some(draft),
            auto_open: false,
            adapt_video,
            audio_policy: StartupAudioPolicy::Explicit,
            bootstrap_audio: Default::default(),
            reason: format!("Saved selection pre-filled, not active: {reason}"),
        },
    }
}

#[derive(Clone, Debug, PartialEq)]
enum CloseState {
    Open,
    Draining,
    SaveFailed,
    DurabilityWarning,
    Allowed,
}

enum ReceiptOrigin {
    User,
    StartupAdaptation {
        /// Captured at admission, never read back from mutable profile state.
        preserved_audio: Option<AudioSelection>,
    },
}
struct AdmittedProfile {
    admission: ApplyAdmission,
    submitted: Draft,
    binding: ProfileBinding,
    origin: ReceiptOrigin,
}

pub struct PersistenceSession {
    store: Option<SettingsStore>,
    preferences: LocalPreferences,
    user_apply: Option<AdmittedProfile>,
    user_open: Option<AttemptKey>,
    profiles: SourceProfiles,
    last_source: Option<DeviceIdentity>,
    close: CloseState,
    reset_token: Option<u64>,
    next_reset_token: u64,
    load_status: String,
    write_status: String,
    durability_unconfirmed: bool,
    startup_reason: String,
    /// Legacy filters were loaded but not yet removed by an authorized save.
    retired_filters_pending: bool,
    /// The load/refusal channel and the write channel are independent: a
    /// successful save never hides an unresolved startup or refusal problem.
    write_needs_attention: bool,
    startup_needs_attention: bool,
}
impl PersistenceSession {
    pub fn load(path: Result<PathBuf, String>) -> Self {
        let mut session = Self {
            store: None,
            preferences: LocalPreferences::default(),
            user_apply: None,
            user_open: None,
            profiles: SourceProfiles::default(),
            last_source: None,
            close: CloseState::Open,
            reset_token: None,
            next_reset_token: 0,
            load_status: String::new(),
            write_status: String::new(),
            startup_reason: String::new(),
            durability_unconfirmed: false,
            retired_filters_pending: false,
            write_needs_attention: false,
            startup_needs_attention: false,
        };
        match path {
            Err(error) => session.load_status = error,
            Ok(path) => {
                let (store, outcome) = SettingsStore::load(path);
                session.load_status = match outcome {
                    LoadOutcome::Missing => "First-use session preferences (not restored); they save at safe application close.".into(),
                    LoadOutcome::Loaded { document, retired_filters_present } => {
                        session.preferences = document.preferences;
                        session.profiles = document.profiles;
                        session.last_source = document.last_source;
                        session.retired_filters_pending = retired_filters_present;
                        if retired_filters_present {
                            "Legacy filters ignored; the other settings are preserved. The legacy filter data will be permanently removed from the settings file, without deleting that file, at the next authorized save. After that v3-format save, older builds will refuse the file and reverting to an older binary will not restore the filters. Before this save, the file bytes remain unchanged.".into()
                        } else {
                            "Saved local preferences restored.".into()
                        }
                    }
                    LoadOutcome::Refused(error) => format!("Settings file refused: {error}. Original preserved. ALL automatic saves are blocked; session preference changes will NOT survive relaunch until explicit reset. Current values are initial session values, not restored."),
                };
                session.store = Some(store);
            }
        }
        session
    }
    /// Borrowed view of the local preferences. The output choice is not
    /// `Copy`: callers reading a single preference field read through this
    /// reference instead of cloning an identity.
    pub fn preferences(&self) -> &LocalPreferences {
        &self.preferences
    }
    pub fn set_preferences(&mut self, preferences: LocalPreferences) {
        self.preferences = preferences;
    }
    /// Replaces the gain preference in place, leaving the output choice
    /// untouched and un-cloned on hot volume/mute paths.
    pub fn set_gain_preference(&mut self, gain: PlaybackGain) {
        self.preferences.gain = gain;
    }
    /// Replaces the fullscreen preference in place.
    pub fn set_fullscreen_preference(&mut self, fullscreen: bool) {
        self.preferences.fullscreen = fullscreen;
    }
    /// Replaces stream-stat visibility without changing other preferences.
    pub fn set_stats_hidden_preference(&mut self, hidden: bool) {
        self.preferences.stats_hidden = hidden;
    }
    /// Replaces the diagnostic-logging opt-in in place. Off is the default for
    /// missing or strictly refused settings; the mutator only records the
    /// session preference and never writes the file eagerly.
    pub fn set_logging_enabled_preference(&mut self, enabled: bool) {
        self.preferences.logging_enabled = enabled;
    }
    pub fn saved_selection(&self) -> Option<DraftSettings> {
        self.saved_selection_ref().cloned()
    }
    /// Lets diagnostics check its work budget before copying private settings.
    pub(crate) fn saved_selection_ref(&self) -> Option<&DraftSettings> {
        let identity = self.last_source.as_ref()?;
        self.profiles
            .entries()
            .iter()
            .find(|profile| &profile.settings().video.identity == identity)
            .map(|profile| profile.settings())
    }
    pub fn source_profile(
        &self,
        identity: &DeviceIdentity,
        serial_unique: bool,
    ) -> Option<&DraftSettings> {
        self.profiles.lookup(identity, serial_unique)
    }
    pub fn observe_profile_identities<'a, I>(&mut self, physical: I)
    where
        I: Iterator<Item = &'a DeviceIdentity> + Clone,
    {
        self.profiles.observe(physical);
    }
    pub fn profile_binding(
        &self,
        identity: &DeviceIdentity,
        serial_unique: bool,
    ) -> ProfileBinding {
        self.profiles.binding_for(identity, serial_unique)
    }
    pub fn saved_binding(&self) -> Option<ProfileBinding> {
        let identity = self.last_source.as_ref()?;
        self.profiles
            .entries()
            .iter()
            .find(|profile| &profile.settings().video.identity == identity)
            .map(|profile| profile.binding())
    }
    /// Independent, non-stale alerts for the settings surface: the load/refusal
    /// channel, the write channel and the startup channel survive each other.
    /// Built directly from borrowed messages, in that order, with no temporary
    /// collection and no empty line for an unset channel.
    pub fn attention(&self) -> String {
        let mut attention = String::new();
        let append = |attention: &mut String, message: &str| {
            if message.is_empty() {
                return;
            }
            if !attention.is_empty() {
                attention.push('\n');
            }
            attention.push_str(message);
        };
        if self.store.is_none() || self.refused() || self.retired_filters_pending {
            append(&mut attention, &self.load_status);
        }
        if self.write_needs_attention {
            append(&mut attention, &self.write_status);
        }
        if self.startup_needs_attention {
            append(&mut attention, &self.startup_reason);
        }
        attention
    }
    pub fn prepare_startup(&mut self, selection: &StartupSelection) {
        self.startup_reason = selection.reason.clone();
        self.startup_needs_attention = selection.restore_profile && !selection.auto_open;
    }
    pub fn startup_failed(&mut self, reason: String) {
        self.startup_reason =
            format!("Saved selection pre-filled, not active: initial restoration failed: {reason}");
        self.startup_needs_attention = true;
    }
    pub fn startup_verified(&mut self) {
        self.startup_reason = "Startup restoration completed: exact capture and initial audio receipt verified. Pause state was not restored.".into();
        self.startup_needs_attention = false;
    }
    pub fn path(&self) -> String {
        self.store
            .as_ref()
            .map(|store| store.path().display().to_string())
            .unwrap_or_default()
    }
    pub fn refused(&self) -> bool {
        self.store.as_ref().is_some_and(SettingsStore::is_refused)
    }
    pub fn status(&self) -> String {
        if self.write_status.is_empty() {
            self.load_status.clone()
        } else {
            format!("{}\n{}", self.load_status, self.write_status)
        }
    }
    pub fn startup_reason(&self) -> &str {
        &self.startup_reason
    }
    pub fn admit_user_apply(
        &mut self,
        admission: ApplyAdmission,
        submitted: Draft,
        binding: ProfileBinding,
    ) {
        self.admit_profile(admission, submitted, binding, ReceiptOrigin::User);
    }
    pub fn admit_startup_adaptation(
        &mut self,
        admission: ApplyAdmission,
        submitted: Draft,
        binding: ProfileBinding,
        audio_policy: StartupAudioPolicy,
    ) {
        let preserved_audio = if audio_policy == StartupAudioPolicy::Cli {
            let identity = &submitted.settings.video.identity;
            let Some(profile) = self.profiles.entries().iter().find(|profile| {
                profile.binding() == binding
                    && (profile.settings().video.identity == *identity
                        || (binding == ProfileBinding::Serial
                            && identity.serial().is_some()
                            && profile.settings().video.identity.serial() == identity.serial()
                            && profile.settings().video.identity.vendor_id()
                                == identity.vendor_id()
                            && profile.settings().video.identity.product_id()
                                == identity.product_id()))
            }) else {
                self.user_apply = None;
                self.user_open = None;
                return;
            };
            Some(profile.settings().audio.clone())
        } else {
            None
        };
        self.admit_profile(
            admission,
            submitted,
            binding,
            ReceiptOrigin::StartupAdaptation { preserved_audio },
        );
    }
    fn admit_profile(
        &mut self,
        admission: ApplyAdmission,
        submitted: Draft,
        binding: ProfileBinding,
        origin: ReceiptOrigin,
    ) {
        self.user_apply = Some(AdmittedProfile {
            admission,
            submitted,
            binding,
            origin,
        });
        self.user_open = None;
    }
    pub fn authorized_apply(&self) -> Option<ApplyId> {
        self.user_apply
            .as_ref()
            .map(|admitted| admitted.admission.id())
    }
    pub fn cancel_user_apply(&mut self, apply: ApplyId) {
        if self.authorized_apply() == Some(apply) {
            self.user_apply = None;
            self.user_open = None;
        }
    }
    pub fn observe_opening(&mut self, key: AttemptKey) {
        if self
            .user_apply
            .as_ref()
            .is_some_and(|admitted| matches!(&admitted.admission, ApplyAdmission::Open { apply } if *apply == key.apply))
            && key.purpose == AttemptPurpose::Candidate
        {
            self.user_open = Some(key);
        }
    }
    pub fn verified_applied(&mut self, event: VerifiedApplied) {
        let Some(AdmittedProfile {
            admission,
            submitted,
            binding,
            origin,
        }) = &self.user_apply
        else {
            return;
        };
        let binding = *binding;
        let same_source = |observed: &DeviceIdentity| {
            let expected = &submitted.settings.video.identity;
            observed == expected
                || (binding == ProfileBinding::Serial
                    && expected.serial().is_some()
                    && expected.serial() == observed.serial()
                    && expected.vendor_id() == observed.vendor_id()
                    && expected.product_id() == observed.product_id())
        };
        let applied = match (admission, event) {
            (ApplyAdmission::Open { apply }, VerifiedApplied::Open { key, applied })
                if *apply == key.apply
                    && key.purpose == AttemptPurpose::Candidate
                    && self.user_open == Some(key)
                    && same_source(&applied.settings().video.identity)
                    && applied.settings().video.mode == submitted.settings.video.mode
                    && applied.settings().video.resolved_mode().is_some()
                    && applied.settings().audio == submitted.settings.audio
                    && applied.settings().renderer == submitted.settings.renderer =>
            {
                applied
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
            ) if *admitted == key
                && matches!(origin, ReceiptOrigin::User)
                && key.pass == crate::domain::state::TreatmentPass::Candidate
                && *admitted_revision == revision
                && submitted.revision == revision
                && applied.settings().same_request(&submitted.settings)
                && applied.settings().video.resolved_mode().is_some() =>
            {
                applied
            }
            _ => return,
        };
        self.last_source = Some(applied.settings().video.identity.clone());
        match origin {
            ReceiptOrigin::StartupAdaptation {
                preserved_audio: Some(audio),
            } => self
                .profiles
                .upsert_verified_preserving_audio(&applied, binding, audio),
            _ => self.profiles.upsert_verified(&applied, binding),
        }
        self.user_apply = None;
        self.user_open = None;
        self.save();
    }
    fn save(&mut self) -> bool {
        let Some(store) = &mut self.store else {
            self.write_status = "Save unavailable: no valid configuration location. Session preferences are not durable.".into();
            self.write_needs_attention = true;
            return false;
        };
        let outcome = store.save_profiles(
            &self.profiles,
            self.last_source.as_ref(),
            self.preferences.clone(),
        );
        match outcome {
            Err(error) => {
                self.write_status = format!(
                    "Save failed at {}: {error}. Previous published file preserved.",
                    store.path().display()
                );
                self.write_needs_attention = true;
                false
            }
            Ok(WriteOutcome::Committed { durability }) => {
                self.durability_unconfirmed = matches!(&durability, Durability::Unconfirmed(_));
                self.write_needs_attention = self.durability_unconfirmed;
                // The document is published in every case, so the pending
                // legacy-removal notice is replaced by its factual outcome.
                if std::mem::take(&mut self.retired_filters_pending) {
                    self.load_status = "Legacy filter data removed by an authorized v3-format save; other settings preserved. Older builds refuse this file, and reverting to an older binary will not restore filters.".into();
                }
                self.write_status = match durability {
                    Durability::Confirmed => format!("Saved to {}.", store.path().display()),
                    Durability::Unconfirmed(error) => format!(
                        "Committed to {}, but crash durability is NOT confirmed: {error}. New complete document published, not the previous file.",
                        store.path().display()
                    ),
                };
                true
            }
        }
    }
    pub fn mutations_open(&self) -> bool {
        self.close == CloseState::Open
    }
    /// Start normal teardown immediately. Only a previously verified pending
    /// receipt (if a save failed) and current preferences can be saved at drain.
    pub fn request_close(&mut self) -> bool {
        if self.close != CloseState::Open {
            return false;
        }
        self.reset_token = None;
        if let Some(apply) = self.authorized_apply() {
            self.cancel_user_apply(apply);
        }
        self.close = CloseState::Draining;
        true
    }
    pub fn close_dialog(&self) -> &'static str {
        match self.close {
            CloseState::SaveFailed => "save",
            CloseState::DurabilityWarning => "warning",
            _ => "",
        }
    }
    pub fn drained(&mut self) {
        if self.close != CloseState::Draining {
            return;
        }
        if self.save() {
            self.close = if self.durability_unconfirmed {
                CloseState::DurabilityWarning
            } else {
                CloseState::Allowed
            };
        } else {
            self.close = CloseState::SaveFailed;
        }
    }
    pub fn retry_close_save(&mut self) {
        if self.close == CloseState::SaveFailed {
            self.close = CloseState::Draining;
            self.drained();
        }
    }
    pub fn close_without_save(&mut self) {
        if matches!(
            self.close,
            CloseState::SaveFailed | CloseState::DurabilityWarning
        ) {
            self.close = CloseState::Allowed;
        }
    }
    pub fn quit_allowed(&self) -> bool {
        self.close == CloseState::Allowed
    }
    pub fn force_close(&mut self) {
        if let Some(apply) = self.authorized_apply() {
            self.cancel_user_apply(apply);
        }
        self.close = CloseState::Allowed;
    }
    pub fn request_reset(&mut self) {
        if self.close != CloseState::Open || self.store.is_none() || self.reset_token.is_some() {
            return;
        }
        if let Some(apply) = self.authorized_apply() {
            self.cancel_user_apply(apply);
        }
        self.next_reset_token += 1;
        self.reset_token = Some(self.next_reset_token);
    }
    pub fn reset_token(&self) -> u64 {
        self.reset_token.unwrap_or(0)
    }
    pub fn decide_reset(&mut self, token: u64, confirmed: bool) {
        if self.reset_token != Some(token) {
            return;
        }
        self.reset_token = None;
        if !confirmed {
            return;
        }
        if let Some(apply) = self.authorized_apply() {
            self.cancel_user_apply(apply);
        }
        let Some(store) = &mut self.store else {
            return;
        };
        match store.reset(self.preferences.clone()) {
            Err(error) => {
                self.write_status = format!(
                    "Reset failed at {}: {error}. Original and refusal preserved.",
                    store.path().display()
                );
                self.write_needs_attention = true;
            }
            Ok(WriteOutcome::Committed { durability }) => {
                // An older in-flight Apply or retry cannot resurrect cleared saved capture.
                self.profiles = SourceProfiles::default();
                self.last_source = None;
                self.user_apply = None;
                self.user_open = None;
                // The published document no longer carries legacy filters, but
                // the startup channel is untouched by a file-only reset.
                self.retired_filters_pending = false;
                self.load_status = "Saved file explicitly reset; capture is not saved. Live capture and selection unchanged.".into();
                self.write_needs_attention = matches!(&durability, Durability::Unconfirmed(_));
                self.write_status = match durability {
                    Durability::Confirmed => {
                        "Reset committed with current session preferences and no source profiles."
                            .into()
                    }
                    Durability::Unconfirmed(error) => format!(
                        "Reset committed (no source profiles), but crash durability is NOT confirmed: {error}"
                    ),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        capture::{
            AudioSelection, CaptureMode, CapturedFourCc, DeviceIdentity, FrameRate, FrameSize,
            ModeRequest, PlaybackGain, UsbTopology,
        },
        state::{DraftRevision, ModelEffect, ProductModel},
    };
    use std::{
        fs,
        num::NonZeroU8,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "furami-settings-app-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self) -> PathBuf {
            self.0.join("settings.json")
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn selection(rate: u32) -> DraftSettings {
        DraftSettings {
            video: crate::domain::capture::VideoRequest::manual(ModeRequest {
                identity: DeviceIdentity::new(
                    0x1234,
                    0xabcd,
                    UsbTopology::new("pci-test".into(), vec![NonZeroU8::new(1).unwrap()]).unwrap(),
                    Some("serial".into()),
                )
                .unwrap(),
                mode: CaptureMode {
                    captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
                    size: FrameSize::new(1280, 720).unwrap(),
                    rate: FrameRate::new(rate, 1001).unwrap(),
                },
            }),
            audio: AudioSelection::default(),
            renderer: crate::domain::render::RendererSettings::default(),
        }
    }
    fn reference_renderer() -> crate::domain::render::RendererSettings {
        use crate::domain::render::{
            DebandValues, DitherMode, RendererSettings, RendererValues, ScalerChoice, ScalerKernel,
        };
        RendererSettings::new(RendererValues {
            scale: ScalerKernel::EwaLanczosSharp,
            cscale: ScalerChoice::Kernel(ScalerKernel::EwaLanczosSharp),
            dscale: ScalerChoice::Kernel(ScalerKernel::Mitchell),
            scale_antiring: 0.7,
            dither: DitherMode::ErrorDiffusion,
            deband: DebandValues {
                enabled: true,
                iterations: 2,
                grain: 16.0,
            },
            interpolation: false,
        })
        .unwrap()
    }
    fn source(serial: &str) -> AudioSourceIdentity {
        AudioSourceIdentity::new(
            "capture-source".into(),
            vec![("device.serial".into(), serial.into())],
        )
        .unwrap()
    }
    /// Open aggregate keyed to the physical opening: the media owner returns it
    /// only after nine native renderer readbacks and matching playback.
    fn verify_open(model: &mut ProductModel, key: crate::domain::state::AttemptKey) {
        let request = model.opening_request().expect("opening request for proof");
        let treatment = crate::domain::state::TreatmentAttemptKey {
            apply: key.apply,
            attempt: key.attempt,
            pass: crate::domain::state::TreatmentPass::Open,
        };
        let confirmation = crate::media::controller::test_support::treatment_confirmation(
            treatment,
            request.playback,
            request.settings.renderer.clone(),
        );
        model.open_verified(key, confirmation);
    }
    fn event(settings: DraftSettings) -> VerifiedApplied {
        let mut model = ProductModel::new(settings);
        model
            .apply(model.state_identity(), model.draft().revision)
            .unwrap();
        let request = model.validation_request().unwrap().clone();
        let Some(ModelEffect::Open { key, .. }) = model.fixture_validation_succeeded(&request)
        else {
            panic!("opening")
        };
        verify_open(&mut model, key);
        VerifiedApplied::Open {
            key,
            applied: model.last_valid().unwrap().clone(),
        }
    }
    fn user_apply(session: &mut PersistenceSession, settings: DraftSettings) {
        let submitted = Draft {
            revision: DraftRevision::new(0),
            settings: settings.clone(),
        };
        let event = event(settings);
        let VerifiedApplied::Open { key, .. } = &event else {
            panic!("Open fixture")
        };
        session.admit_user_apply(
            ApplyAdmission::Open { apply: key.apply },
            submitted,
            ProfileBinding::Serial,
        );
        session.observe_opening(*key);
        session.verified_applied(event);
    }
    struct LoadedSelection {
        applied: Option<DraftSettings>,
        preferences: LocalPreferences,
    }
    fn load_document(path: PathBuf) -> LoadedSelection {
        let (_, LoadOutcome::Loaded { document, .. }) = SettingsStore::load(path) else {
            panic!("document not loadable")
        };
        let applied = document.last_source.as_ref().and_then(|identity| {
            document
                .profiles
                .entries()
                .iter()
                .find(|profile| &profile.settings().video.identity == identity)
                .map(|profile| profile.settings().clone())
        });
        LoadedSelection {
            applied,
            preferences: document.preferences,
        }
    }
    fn preferences(percent: u8) -> LocalPreferences {
        LocalPreferences {
            gain: PlaybackGain::new(percent, true).unwrap(),
            fullscreen: true,
            ..LocalPreferences::default()
        }
    }
    /// A saved selection that could not auto-open: the only startup shape that
    /// raises the startup attention channel.
    fn prefill_selection(reason: &str) -> StartupSelection {
        StartupSelection {
            draft: Some(selection(60000)),
            auto_open: false,
            adapt_video: false,
            restore_profile: true,
            source_binding: None,
            topology_pin: None,
            reason: reason.into(),
            audio_policy: StartupAudioPolicy::Explicit,
            bootstrap_audio: Default::default(),
        }
    }
    /// Rebuilds a schema-2 document that still carries legacy `filters` data.
    fn legacy_filter_bytes(seed: &Temp) -> Vec<u8> {
        let mut original = PersistenceSession::load(Ok(seed.file()));
        let mut saved = selection(60000);
        saved.renderer = reference_renderer();
        original.set_preferences(preferences(29));
        user_apply(&mut original, saved);
        let mut wire: serde_json::Value =
            serde_json::from_slice(&fs::read(seed.file()).unwrap()).unwrap();
        wire["schema_version"] = 2.into();
        wire["profiles"][0]["settings"]["filters"] = serde_json::json!({"invalid": true});
        serde_json::to_vec(&wire).unwrap()
    }

    #[test]
    fn xdg_resolution_never_writes_relative_or_unresolvable_location() {
        assert_eq!(
            resolve_settings_path(Some(OsStr::new("/xdg")), None).unwrap(),
            PathBuf::from("/xdg/furami/settings.json")
        );
        assert_eq!(
            resolve_settings_path(Some(OsStr::new("")), Some(OsStr::new("/home/test"))).unwrap(),
            PathBuf::from("/home/test/.config/furami/settings.json")
        );
        assert!(
            resolve_settings_path(Some(OsStr::new("relative")), Some(OsStr::new("/valid")))
                .is_err()
        );
        assert!(resolve_settings_path(None, Some(OsStr::new("relative"))).is_err());
        assert!(resolve_settings_path(None, None).is_err());
    }

    #[test]
    fn saved_auto_rechecks_current_capabilities_without_historical_pin() {
        use crate::domain::capture::InputFormatPolicy;
        let mut saved = selection(60000);
        saved.video.mode.format_policy = InputFormatPolicy::Auto;
        let startup = decide_startup(
            Some(saved.clone()),
            None,
            |draft| {
                assert_eq!(draft.video.mode.format_policy, InputFormatPolicy::Auto);
                assert!(draft.video.resolved_fourcc.is_none());
                Ok(())
            },
            Ok(&[]),
        );
        assert!(startup.auto_open);
        assert_eq!(startup.audio_policy, StartupAudioPolicy::Explicit);
        let restored = startup.draft.unwrap();
        assert_eq!(restored.video.mode, saved.video.mode);
        assert!(restored.video.resolved_fourcc.is_none());
    }
    #[test]
    fn startup_present_exact_tuple_opens_but_absent_unknown_or_unsupported_prefills() {
        let saved = selection(60000);
        let available = decide_startup(Some(saved.clone()), None, |_| Ok(()), Ok(&[]));
        assert!(available.auto_open);
        assert_eq!(available.draft, Some(saved.clone()));
        for reason in [
            "device absent",
            "ambiguous serial",
            "inventory unavailable",
            "exact tuple unsupported",
        ] {
            let unavailable =
                decide_startup(Some(saved.clone()), None, |_| Err(reason.into()), Ok(&[]));
            assert!(!unavailable.auto_open);
            assert_eq!(unavailable.draft, Some(saved.clone()));
            assert!(unavailable.reason.contains(reason));
        }
    }
    #[test]
    fn enabled_audio_requires_exact_stable_identity_and_catalog() {
        let mut saved = selection(60000);
        saved.audio = AudioSelection::Enabled {
            source: source("expected"),
        };
        let sources = vec![source("different")];
        assert!(!decide_startup(Some(saved.clone()), None, |_| Ok(()), Ok(&sources)).auto_open);
        assert!(
            !decide_startup(
                Some(saved.clone()),
                None,
                |_| Ok(()),
                Err("catalog failed".into())
            )
            .auto_open
        );
        assert!(!decide_startup(Some(saved.clone()), None, |_| Ok(()), Ok(&[])).auto_open);
        let sources = vec![source("expected")];
        assert!(decide_startup(Some(saved), None, |_| Ok(()), Ok(&sources)).auto_open);
    }
    #[test]
    fn disabled_retained_audio_absence_does_not_block_startup() {
        let mut saved = selection(60000);
        saved.audio = AudioSelection::Disabled {
            retained: Some(source("gone")),
        };
        assert!(
            decide_startup(
                Some(saved),
                None,
                |_| Ok(()),
                Err("catalog unavailable".into())
            )
            .auto_open
        );
    }
    #[test]
    fn explicit_cli_selection_overrides_saved_auto_open_without_preflight_or_save() {
        let explicit = selection(30000);
        let chosen = decide_startup(
            Some(selection(60000)),
            Some(explicit.clone()),
            |_| panic!("explicit CLI is not saved auto-open"),
            Err("absent".into()),
        );
        assert!(!chosen.auto_open);
        assert_eq!(chosen.draft, Some(explicit));
    }
    #[test]
    fn confirmed_renderer_saves_complete_settings_for_both_receipt_variants() {
        use crate::domain::state::{TreatmentAttemptKey, TreatmentPass};
        for live_renderer in [false, true] {
            let temp = Temp::new();
            let mut session = PersistenceSession::load(Ok(temp.file()));
            let mut submitted = selection(60000);
            submitted.renderer = reference_renderer();
            let VerifiedApplied::Open { key, applied } = event(submitted.clone()) else {
                panic!("Open fixture")
            };
            let revision = DraftRevision::new(1);
            let (admission, receipt) = if live_renderer {
                let key = TreatmentAttemptKey {
                    apply: key.apply,
                    attempt: key.attempt,
                    pass: TreatmentPass::Candidate,
                };
                (
                    ApplyAdmission::Treatments { key, revision },
                    VerifiedApplied::Treatments {
                        key,
                        revision,
                        applied,
                    },
                )
            } else {
                (
                    ApplyAdmission::Open { apply: key.apply },
                    VerifiedApplied::Open { key, applied },
                )
            };
            session.admit_user_apply(
                admission,
                Draft {
                    revision,
                    settings: submitted.clone(),
                },
                ProfileBinding::Serial,
            );
            session.observe_opening(key);
            session.verified_applied(receipt.clone());
            assert_eq!(load_document(temp.file()).applied, Some(submitted.clone()));
            assert_eq!(session.authorized_apply(), None);
            let bytes = fs::read(temp.file()).unwrap();
            session.verified_applied(receipt);
            assert_eq!(fs::read(temp.file()).unwrap(), bytes);
        }
    }

    #[test]
    fn immutable_renderer_admission_cannot_authorize_an_open_receipt_and_cancel_consumes_permission()
     {
        use crate::domain::state::{TreatmentAttemptKey, TreatmentPass};
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        let receipt = event(selection(60000));
        let VerifiedApplied::Open { key: open, .. } = &receipt else {
            panic!("Open fixture")
        };
        let key = TreatmentAttemptKey {
            apply: open.apply,
            attempt: open.attempt,
            pass: TreatmentPass::Candidate,
        };
        session.admit_user_apply(
            ApplyAdmission::Treatments {
                key,
                revision: DraftRevision::new(3),
            },
            Draft {
                revision: DraftRevision::new(3),
                settings: selection(60000),
            },
            ProfileBinding::Serial,
        );
        session.observe_opening(*open);
        session.verified_applied(receipt.clone());
        assert!(!temp.file().exists());
        assert_eq!(session.authorized_apply(), Some(key.apply));
        session.cancel_user_apply(ApplyId::new(key.apply.get() + 1).unwrap());
        assert_eq!(session.authorized_apply(), Some(key.apply));
        session.cancel_user_apply(key.apply);
        assert_eq!(session.authorized_apply(), None);
        session.admit_user_apply(
            ApplyAdmission::Open { apply: key.apply },
            Draft {
                revision: DraftRevision::new(4),
                settings: selection(60000),
            },
            ProfileBinding::Serial,
        );
        session.observe_opening(*open);
        session.cancel_user_apply(key.apply);
        session.verified_applied(receipt);
        assert!(!temp.file().exists());
    }

    #[test]
    fn immutable_renderer_receipts_require_exact_key_revision_and_complete_submission() {
        use crate::domain::state::{AttemptId, TreatmentAttemptKey, TreatmentPass};
        let VerifiedApplied::Open { key: open, applied } = event(selection(60000)) else {
            panic!("fixture")
        };
        let key = TreatmentAttemptKey {
            apply: open.apply,
            attempt: open.attempt,
            pass: TreatmentPass::Candidate,
        };
        let revision = DraftRevision::new(3);
        for mismatch in 0..8 {
            let temp = Temp::new();
            let mut session = PersistenceSession::load(Ok(temp.file()));
            session.admit_user_apply(
                ApplyAdmission::Treatments { key, revision },
                Draft {
                    revision: if mismatch == 5 {
                        DraftRevision::new(4)
                    } else {
                        revision
                    },
                    settings: selection(60000),
                },
                ProfileBinding::Serial,
            );
            let mut received = key;
            let mut received_revision = revision;
            let mut received_applied = applied.clone();
            match mismatch {
                0 => received.apply = ApplyId::new(key.apply.get() + 1).unwrap(),
                1 => received.attempt = AttemptId::new(key.attempt.get() + 1).unwrap(),
                2 => received.pass = TreatmentPass::Restore,
                3 => received.pass = TreatmentPass::Open,
                4 => received_revision = DraftRevision::new(4),
                5 => {}
                6 => {
                    let VerifiedApplied::Open { applied: other, .. } = event(selection(30000))
                    else {
                        panic!("fixture")
                    };
                    received_applied = other;
                }
                7 => {
                    // Only the renderer differs from the submitted selection:
                    // the immutable admission still refuses to authorize it.
                    let mut different = selection(60000);
                    different.renderer = reference_renderer();
                    let VerifiedApplied::Open { applied: other, .. } = event(different) else {
                        panic!("fixture")
                    };
                    received_applied = other;
                }
                _ => unreachable!(),
            }
            session.verified_applied(VerifiedApplied::Treatments {
                key: received,
                revision: received_revision,
                applied: received_applied,
            });
            assert!(!temp.file().exists(), "mismatch {mismatch} must not write");
            assert_eq!(session.authorized_apply(), Some(key.apply));
            assert!(session.profiles.entries().is_empty());
        }
    }
    #[test]
    fn exact_renderer_receipt_saves_once_and_cancellation_revokes_it() {
        use crate::domain::state::{TreatmentAttemptKey, TreatmentPass};
        let VerifiedApplied::Open { key: open, applied } = event(selection(60000)) else {
            panic!("fixture")
        };
        let key = TreatmentAttemptKey {
            apply: open.apply,
            attempt: open.attempt,
            pass: TreatmentPass::Candidate,
        };
        let revision = DraftRevision::new(3);
        let receipt = VerifiedApplied::Treatments {
            key,
            revision,
            applied,
        };
        for cancelled in [false, true] {
            let temp = Temp::new();
            let mut session = PersistenceSession::load(Ok(temp.file()));
            session.admit_user_apply(
                ApplyAdmission::Treatments { key, revision },
                Draft {
                    revision,
                    settings: selection(60000),
                },
                ProfileBinding::Serial,
            );
            if cancelled {
                session.cancel_user_apply(key.apply);
            }
            session.verified_applied(receipt.clone());
            assert_eq!(temp.file().exists(), !cancelled);
            assert_eq!(session.authorized_apply(), None);
            if !cancelled {
                assert_eq!(load_document(temp.file()).applied, Some(selection(60000)));
                let bytes = fs::read(temp.file()).unwrap();
                session.verified_applied(receipt.clone());
                assert_eq!(fs::read(temp.file()).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn only_exact_correlated_user_receipt_saves_once_not_startup_restart_or_rollback() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        session.verified_applied(event(selection(60000)));
        assert!(!temp.file().exists());
        let mut receipt = event(selection(60000));
        let VerifiedApplied::Open { key, .. } = &mut receipt else {
            panic!("Open fixture")
        };
        session.admit_user_apply(
            ApplyAdmission::Open { apply: key.apply },
            Draft {
                revision: DraftRevision::new(0),
                settings: selection(60000),
            },
            ProfileBinding::Serial,
        );
        let wrong_key = AttemptKey {
            attempt: crate::domain::state::AttemptId::new(key.attempt.get() + 1).unwrap(),
            ..*key
        };
        session.observe_opening(wrong_key);
        session.verified_applied(event(selection(60000)));
        assert!(!temp.file().exists());
        key.purpose = AttemptPurpose::Restore;
        session.verified_applied(receipt);
        assert!(!temp.file().exists());
        user_apply(&mut session, selection(60000));
        assert_eq!(load_document(temp.file()).applied, Some(selection(60000)));
        let bytes = fs::read(temp.file()).unwrap();
        session.verified_applied(event(selection(30000)));
        assert_eq!(fs::read(temp.file()).unwrap(), bytes);
    }
    #[test]
    fn close_drains_once_without_saving_uncommitted_startup_selection() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        let startup = decide_startup(None, Some(selection(30000)), |_| Ok(()), Ok(&[]));
        session.prepare_startup(&startup);
        session.set_preferences(preferences(17));
        assert!(session.request_close());
        assert!(!session.request_close());
        assert!(!session.mutations_open());
        assert_eq!(session.close_dialog(), "");
        assert!(!session.quit_allowed());
        session.drained();
        assert!(session.quit_allowed());
        let document = load_document(temp.file());
        assert_eq!(document.applied, None);
        assert_eq!(document.preferences, preferences(17));
    }
    #[test]
    fn close_saves_only_verified_applied_and_latest_preferences_after_drain() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        user_apply(&mut session, selection(60000));
        session.set_preferences(preferences(23));
        assert!(session.request_close());
        assert!(!session.quit_allowed());
        session.drained();
        assert!(session.quit_allowed());
        let document = load_document(temp.file());
        assert_eq!(document.applied, Some(selection(60000)));
        assert_eq!(document.preferences, preferences(23));
    }
    #[test]
    fn legacy_settings_load_without_writing_and_migrate_only_at_safe_close() {
        for version in [1, 2, 3] {
            for retired in [false, true] {
                let temp = Temp::new();
                let mut saved = selection(60000);
                saved.renderer = reference_renderer();
                let mut original = PersistenceSession::load(Ok(temp.file()));
                let mut globals = preferences(29);
                globals.stats_hidden = true;
                globals.logging_enabled = true;
                original.set_preferences(globals.clone());
                user_apply(&mut original, saved.clone());
                let mut wire: serde_json::Value =
                    serde_json::from_slice(&fs::read(temp.file()).unwrap()).unwrap();
                wire["schema_version"] = version.into();
                if retired {
                    wire["profiles"][0]["settings"]["filters"] =
                        serde_json::json!({"invalid": true});
                }
                if version == 1 {
                    let applied = wire["profiles"][0]["settings"].take();
                    let object = wire.as_object_mut().unwrap();
                    object.remove("profiles");
                    object.remove("last_source");
                    object.insert("applied".into(), applied);
                }
                let bytes = serde_json::to_vec(&wire).unwrap();
                fs::write(temp.file(), &bytes).unwrap();
                let mut loaded = PersistenceSession::load(Ok(temp.file()));
                assert_eq!(fs::read(temp.file()).unwrap(), bytes);
                if retired && version == 3 {
                    assert!(loaded.refused());
                    assert!(loaded.saved_selection().is_none());
                    assert!(!loaded.preferences().logging_enabled);
                    assert!(loaded.attention().contains("refused"));
                    assert!(loaded.request_close());
                    loaded.drained();
                    assert!(!loaded.quit_allowed());
                    assert_eq!(fs::read(temp.file()).unwrap(), bytes);
                    assert!(loaded.attention().contains("refused"));
                    continue;
                }
                assert_eq!(loaded.saved_selection(), Some(saved.clone()));
                assert_eq!(loaded.preferences(), &globals);
                if retired {
                    assert!(loaded.attention().contains("Legacy filters ignored"));
                } else {
                    assert!(loaded.attention().is_empty());
                }
                assert!(loaded.request_close());
                assert_eq!(fs::read(temp.file()).unwrap(), bytes);
                loaded.drained();
                assert!(loaded.quit_allowed());
                assert!(
                    loaded.attention().is_empty(),
                    "a confirmed v3 publication clears the pending-migration alert"
                );
                let written: serde_json::Value =
                    serde_json::from_slice(&fs::read(temp.file()).unwrap()).unwrap();
                assert_eq!(written["schema_version"], 3);
                assert!(written["profiles"][0]["settings"].get("filters").is_none());
                let reloaded = PersistenceSession::load(Ok(temp.file()));
                assert_eq!(reloaded.saved_selection(), Some(saved));
                assert_eq!(reloaded.preferences(), &globals);
            }
        }
    }
    #[test]
    fn preference_only_close_without_capture_restores_latest_preferences_and_null_capture() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        session.set_preferences(preferences(41));
        session.set_stats_hidden_preference(true);
        assert!(session.request_close());
        session.drained();
        assert!(session.quit_allowed());
        let reloaded = PersistenceSession::load(Ok(temp.file()));
        let mut expected = preferences(41);
        expected.stats_hidden = true;
        assert_eq!(reloaded.preferences(), &expected);
        assert!(reloaded.saved_selection().is_none());
    }
    #[test]
    fn refused_original_survives_apply_preferences_close_and_cancelled_reset() {
        for original in [
            b"not json".as_slice(),
            br#"{"schema_version":999,"future":true}"#.as_slice(),
        ] {
            let temp = Temp::new();
            fs::write(temp.file(), original).unwrap();
            let mut session = PersistenceSession::load(Ok(temp.file()));
            assert!(session.refused() && session.status().contains("NOT survive"));
            session.set_preferences(preferences(7));
            user_apply(&mut session, selection(60000));
            session.request_reset();
            session.decide_reset(session.reset_token(), false);
            assert_eq!(fs::read(temp.file()).unwrap(), original);
            assert!(session.request_close());
            session.drained();
            assert!(!session.quit_allowed());
            assert_eq!(session.close_dialog(), "save");
            session.retry_close_save();
            assert_eq!(fs::read(temp.file()).unwrap(), original);
            session.close_without_save();
            assert!(session.quit_allowed());
        }
    }
    #[test]
    fn explicit_reset_is_file_only_current_preferences_and_clears_old_apply_retry() {
        let temp = Temp::new();
        fs::write(temp.file(), b"refused").unwrap();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        user_apply(&mut session, selection(60000));
        session.set_preferences(preferences(12));
        session.set_stats_hidden_preference(true);
        session.request_reset();
        let token = session.reset_token();
        let refused_attention = session.attention();
        assert!(refused_attention.contains("refused"));
        session.decide_reset(token + 1, true);
        assert!(session.refused());
        assert_eq!(
            session.attention(),
            refused_attention,
            "a stale token leaves every channel untouched"
        );
        session.decide_reset(token, true);
        assert!(!session.refused());
        assert!(
            session.attention().is_empty(),
            "a confirmed reset clears refusal and write attention together"
        );
        let mut expected = preferences(12);
        expected.stats_hidden = true;
        assert_eq!(session.preferences(), &expected);
        assert_eq!(load_document(temp.file()).applied, None);
        assert!(load_document(temp.file()).preferences.stats_hidden);
        session.decide_reset(token, true);
        session.request_close();
        session.drained();
        assert_eq!(load_document(temp.file()).applied, None);
    }
    #[test]
    fn attention_stays_empty_until_a_channel_actually_needs_it() {
        // First use: the informational load message is not an alert.
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        assert!(session.attention().is_empty());
        assert!(session.request_close());
        session.drained();
        assert!(session.quit_allowed());
        assert!(session.attention().is_empty());
        let restored = PersistenceSession::load(Ok(temp.file()));
        assert!(restored.attention().is_empty());

        // An active prefill that can open by itself is not an alert either.
        let mut session = PersistenceSession::load(Ok(temp.file()));
        let mut auto = prefill_selection("Saved sources are available; restoring live capture.");
        auto.auto_open = true;
        session.prepare_startup(&auto);
        assert!(session.attention().is_empty());
        // No saved capture at all is never an alert, whatever it explains.
        let mut none = prefill_selection("No saved capture selection.");
        none.restore_profile = false;
        none.draft = None;
        session.prepare_startup(&none);
        assert!(session.attention().is_empty());
    }
    #[test]
    fn invalid_location_and_refused_file_are_alerts_and_preserve_their_bytes() {
        let invalid = PersistenceSession::load(Err("no usable configuration location".into()));
        assert_eq!(invalid.attention(), invalid.status());
        assert!(
            invalid
                .attention()
                .contains("no usable configuration location")
        );

        for original in [
            b"not json".as_slice(),
            br#"{"schema_version":999,"future":true}"#.as_slice(),
        ] {
            let temp = Temp::new();
            fs::write(temp.file(), original).unwrap();
            let session = PersistenceSession::load(Ok(temp.file()));
            assert!(session.refused());
            assert_eq!(session.attention(), session.status());
            assert!(session.attention().contains("NOT survive"));
            assert_eq!(fs::read(temp.file()).unwrap(), original);
        }
    }
    #[test]
    fn startup_attention_toggles_independently_of_an_unresolved_write_problem() {
        let temp = Temp::new();
        let blocked = temp.0.join("blocked");
        let mut session = PersistenceSession::load(Ok(blocked.join("settings.json")));
        fs::write(&blocked, b"not a directory").unwrap();
        user_apply(&mut session, selection(60000));
        assert!(session.attention().contains("Save failed"));
        session.prepare_startup(&prefill_selection(
            "Saved selection pre-filled, not active: device absent.",
        ));
        let both = session.attention();
        assert!(both.contains("Save failed"));
        assert!(both.contains("device absent"));
        assert_eq!(both.lines().count(), 2, "one line per raised channel");
        // A later failure keeps both; a verified startup clears only its own.
        session.startup_failed("device absent".into());
        assert!(session.attention().contains("initial restoration failed"));
        assert!(session.attention().contains("Save failed"));
        session.startup_verified();
        assert!(!session.attention().contains("verified"));
        assert_eq!(session.attention().lines().count(), 1);
        assert!(session.attention().contains("Save failed"));
        // Fixing the write clears the last channel without touching the rest.
        fs::remove_file(&blocked).unwrap();
        assert!(session.request_close());
        session.drained();
        assert!(session.quit_allowed());
        assert!(session.attention().is_empty());
    }
    #[test]
    fn reset_cancellation_failure_and_success_keep_the_startup_channel() {
        use crate::settings::WriteFaultPoint;
        let temp = Temp::new();
        fs::write(temp.file(), b"refused").unwrap();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        user_apply(&mut session, selection(60000));
        session.prepare_startup(&prefill_selection(
            "Saved selection pre-filled, not active: device absent.",
        ));
        let initial = session.attention();
        assert!(initial.contains("refused"));
        assert!(initial.contains("device absent"));
        // A cancelled confirmation and a stale token change nothing at all.
        session.request_reset();
        let token = session.reset_token();
        session.decide_reset(token, false);
        assert_eq!(session.attention(), initial);
        session.request_reset();
        let token = session.reset_token();
        session.decide_reset(token + 1, true);
        assert_eq!(session.attention(), initial);
        assert!(session.refused());
        // The pending request now fails: refusal and write problems both stay.
        session.store = Some(SettingsStore::with_injected_fault(
            temp.file(),
            WriteFaultPoint::BeforeRename,
        ));
        session.decide_reset(token, true);
        assert!(session.refused());
        let failed = session.attention();
        assert!(failed.contains("refused"));
        assert!(failed.contains("Reset failed"));
        assert!(failed.contains("device absent"));
        // A confirmed reset clears the persistence channels, never startup's.
        session.request_reset();
        let token = session.reset_token();
        session.decide_reset(token, true);
        assert!(!session.refused());
        let after = session.attention();
        assert!(!after.contains("refused"));
        assert!(!after.contains("Reset failed"));
        assert!(after.contains("device absent"));
    }
    #[test]
    fn unconfirmed_reset_keeps_its_durability_warning_and_the_startup_channel() {
        use crate::settings::WriteFaultPoint;
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        session.prepare_startup(&prefill_selection(
            "Saved selection pre-filled, not active: device absent.",
        ));
        session.store = Some(SettingsStore::with_injected_fault(
            temp.file(),
            WriteFaultPoint::ParentSyncIo,
        ));
        session.request_reset();
        let token = session.reset_token();
        session.decide_reset(token, true);
        let attention = session.attention();
        assert!(attention.contains("durability is NOT confirmed"));
        assert!(attention.contains("device absent"));
        assert!(session.status().contains("Reset committed"));
    }
    #[test]
    fn legacy_notice_is_replaced_only_by_a_published_outcome() {
        use crate::settings::WriteFaultPoint;
        let temp = Temp::new();
        let bytes = legacy_filter_bytes(&Temp::new());
        fs::write(temp.file(), &bytes).unwrap();

        // Loading never rewrites the file; the pending migration is an alert.
        let mut session = PersistenceSession::load(Ok(temp.file()));
        assert!(session.attention().contains("Legacy filters ignored"));
        assert!(session.status().contains("will be permanently removed"));
        assert_eq!(fs::read(temp.file()).unwrap(), bytes);

        // A failed save publishes nothing and keeps the pending-migration notice.
        session.store = Some(SettingsStore::with_injected_fault(
            temp.file(),
            WriteFaultPoint::BeforeRename,
        ));
        assert!(session.request_close());
        session.drained();
        assert!(!session.quit_allowed());
        assert_eq!(fs::read(temp.file()).unwrap(), bytes);
        let failed = session.attention();
        assert!(failed.contains("Legacy filters ignored"));
        assert!(failed.contains("Save failed"));

        // A confirmed publication replaces it with the factual outcome.
        let (store, _) = SettingsStore::load(temp.file());
        session.store = Some(store);
        session.retry_close_save();
        assert!(session.quit_allowed());
        assert!(
            session.attention().is_empty(),
            "the confirmed v3 save clears the alert"
        );
        assert!(
            session
                .status()
                .contains("Legacy filter data removed by an authorized v3-format save")
        );
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.file()).unwrap()).unwrap();
        assert_eq!(written["schema_version"], 3);
        assert!(written["profiles"][0]["settings"].get("filters").is_none());
    }
    #[test]
    fn unconfirmed_publication_drops_the_migration_notice_but_keeps_durability_attention() {
        use crate::settings::WriteFaultPoint;
        let temp = Temp::new();
        let bytes = legacy_filter_bytes(&Temp::new());
        fs::write(temp.file(), &bytes).unwrap();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        assert!(session.attention().contains("Legacy filters ignored"));
        session.store = Some(SettingsStore::with_injected_fault(
            temp.file(),
            WriteFaultPoint::ParentSyncIo,
        ));
        assert!(session.request_close());
        session.drained();
        assert_eq!(session.close_dialog(), "warning");
        let attention = session.attention();
        assert!(!attention.contains("will be permanently removed"));
        assert!(!attention.contains("Legacy filters ignored"));
        assert!(attention.contains("durability is NOT confirmed"));
        assert!(
            session
                .status()
                .contains("Legacy filter data removed by an authorized v3-format save")
        );
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.file()).unwrap()).unwrap();
        assert_eq!(written["schema_version"], 3);
        assert!(written["profiles"][0]["settings"].get("filters").is_none());
    }
    #[test]
    fn logging_preference_is_off_by_default_and_safe_close_omits_the_key() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        assert!(!session.preferences().logging_enabled);
        session.set_preferences(preferences(17));
        assert!(session.request_close());
        session.drained();
        assert!(session.quit_allowed());
        let bytes = fs::read(temp.file()).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("logging_enabled"),
            "off must not be written: {}",
            String::from_utf8_lossy(&bytes)
        );
        let reloaded = PersistenceSession::load(Ok(temp.file()));
        assert!(!reloaded.preferences().logging_enabled);
    }
    #[test]
    fn logging_opt_in_defers_to_safe_close_and_survives_relaunch_without_capture() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        session.set_preferences(preferences(41));
        session.set_logging_enabled_preference(true);
        // The session mutator never writes eagerly; durability waits for close.
        assert!(!temp.file().exists());
        assert!(session.request_close());
        session.drained();
        assert!(session.quit_allowed());
        let reloaded = PersistenceSession::load(Ok(temp.file()));
        let mut expected = preferences(41);
        expected.logging_enabled = true;
        assert_eq!(reloaded.preferences(), &expected);
        assert!(reloaded.saved_selection().is_none());
    }
    #[test]
    fn refused_settings_start_off_and_reset_preserves_the_logging_opt_in() {
        let temp = Temp::new();
        fs::write(temp.file(), b"refused").unwrap();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        assert!(session.refused());
        assert!(!session.preferences().logging_enabled);
        session.set_preferences(preferences(12));
        session.set_logging_enabled_preference(true);
        session.request_reset();
        let token = session.reset_token();
        session.decide_reset(token, true);
        let mut expected = preferences(12);
        expected.logging_enabled = true;
        assert_eq!(session.preferences(), &expected);
        let document = load_document(temp.file());
        assert_eq!(document.applied, None);
        assert_eq!(document.preferences, expected);
        session.request_close();
        session.drained();
        assert_eq!(load_document(temp.file()).applied, None);
    }
    #[test]
    fn failed_apply_save_retries_authorized_snapshot_with_latest_preferences_after_drain() {
        let temp = Temp::new();
        let blocked = temp.0.join("blocked");
        let path = blocked.join("settings.json");
        let mut session = PersistenceSession::load(Ok(path.clone()));
        fs::write(&blocked, b"not a directory").unwrap();
        user_apply(&mut session, selection(60000));
        assert_eq!(session.profiles.entries().len(), 1);
        assert!(session.status().contains("Save failed"));
        assert!(session.attention().contains("Save failed"));
        session.set_preferences(preferences(9));
        session.prepare_startup(&prefill_selection(
            "Saved selection pre-filled, not active: device absent.",
        ));
        assert!(session.attention().contains("device absent"));
        assert!(session.request_close());
        session.drained();
        assert_eq!(session.close_dialog(), "save");
        fs::remove_file(&blocked).unwrap();
        session.retry_close_save();
        assert!(session.quit_allowed());
        assert!(
            !session.attention().contains("Save failed"),
            "a confirmed save clears only the write channel"
        );
        assert!(session.attention().contains("device absent"));
        let document = load_document(path);
        assert_eq!(document.applied, Some(selection(60000)));
        assert_eq!(document.preferences, preferences(9));
    }

    #[test]
    fn confirmed_renderer_save_faults_preserve_original_and_retry_frozen_applied_settings() {
        use crate::{
            domain::state::{TreatmentAttemptKey, TreatmentPass},
            settings::WriteFaultPoint,
        };
        for fault in [
            WriteFaultPoint::BeforeWrite,
            WriteFaultPoint::PartialWrite,
            WriteFaultPoint::BeforeTemporarySync,
            WriteFaultPoint::TemporarySyncIo,
            WriteFaultPoint::BeforeRename,
            WriteFaultPoint::RenameIo,
        ] {
            let temp = Temp::new();
            let mut session = PersistenceSession::load(Ok(temp.file()));
            user_apply(&mut session, selection(60000));
            let old = fs::read(temp.file()).unwrap();
            session.store = Some(SettingsStore::with_injected_fault(temp.file(), fault));
            let mut submitted = selection(60000);
            submitted.renderer = reference_renderer();
            let VerifiedApplied::Open { key: open, applied } = event(submitted.clone()) else {
                panic!("fixture")
            };
            let key = TreatmentAttemptKey {
                apply: open.apply,
                attempt: open.attempt,
                pass: TreatmentPass::Candidate,
            };
            let revision = DraftRevision::new(2);
            session.admit_user_apply(
                ApplyAdmission::Treatments { key, revision },
                Draft {
                    revision,
                    settings: submitted.clone(),
                },
                ProfileBinding::Serial,
            );
            session.verified_applied(VerifiedApplied::Treatments {
                key,
                revision,
                applied,
            });
            assert_eq!(session.authorized_apply(), None);
            assert_eq!(fs::read(temp.file()).unwrap(), old, "fault {fault:?}");
            assert!(session.status().contains("Save failed"));
            assert!(
                !session.profiles.entries().is_empty(),
                "runtime success survives persistence refusal"
            );
            session.set_preferences(preferences(23));
            assert!(session.request_close());
            // Each injected fault is one-shot. Fail the safe-close attempt too
            // so the explicit retry below, not automatic close, publishes it.
            session.store = Some(SettingsStore::with_injected_fault(temp.file(), fault));
            session.drained();
            assert_eq!(fs::read(temp.file()).unwrap(), old, "close fault {fault:?}");
            assert!(!session.profiles.entries().is_empty());
            assert!(
                !session.quit_allowed(),
                "failed close save must retain the quit barrier"
            );
            assert_eq!(session.close_dialog(), "save");
            session.retry_close_save();
            assert!(session.quit_allowed());
            let document = load_document(temp.file());
            assert_eq!(document.applied, Some(submitted.clone()));
            assert_eq!(document.preferences, preferences(23));
            // A later uncommitted renderer cannot replace the frozen receipt.
            assert_eq!(
                document.applied.as_ref().unwrap().renderer,
                reference_renderer()
            );
        }
    }

    #[test]
    fn observed_open_key_cannot_authorize_changed_or_dropped_full_submission() {
        use crate::domain::render::{
            DebandValues, DitherMode, RendererSettings, RendererValues, ScalerChoice, ScalerKernel,
        };
        let mut submitted = selection(60000);
        submitted.renderer = reference_renderer();
        for mismatch in 0..4 {
            let temp = Temp::new();
            let mut session = PersistenceSession::load(Ok(temp.file()));
            let mut wrong = submitted.clone();
            match mismatch {
                0 => wrong.renderer = RendererSettings::default(),
                1 => wrong.video.mode = selection(30000).video.mode,
                2 => {
                    wrong.audio = AudioSelection::Enabled {
                        source: source("unexpected"),
                    }
                }
                3 => {
                    wrong.renderer = RendererSettings::new(RendererValues {
                        scale: ScalerKernel::Mitchell,
                        cscale: ScalerChoice::Inherit,
                        dscale: ScalerChoice::Inherit,
                        scale_antiring: 0.5,
                        dither: DitherMode::Ordered,
                        deband: DebandValues {
                            enabled: true,
                            iterations: 2,
                            grain: 16.0,
                        },
                        interpolation: false,
                    })
                    .unwrap()
                }
                _ => unreachable!(),
            }
            let receipt = event(wrong);
            let VerifiedApplied::Open { key, .. } = &receipt else {
                panic!("fixture")
            };
            session.admit_user_apply(
                ApplyAdmission::Open { apply: key.apply },
                Draft {
                    revision: DraftRevision::new(1),
                    settings: submitted.clone(),
                },
                ProfileBinding::Serial,
            );
            session.observe_opening(*key);
            session.verified_applied(receipt);
            assert!(!temp.file().exists());
            assert!(session.profiles.entries().is_empty());
        }
    }
    fn qualify_relocated_user_apply(edit_during_apply: bool) {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        let submitted = selection(60000);
        let mut normalized = submitted.clone();
        normalized.video.identity = DeviceIdentity::new(
            0x1234,
            0xabcd,
            UsbTopology::new("pci-test".into(), vec![NonZeroU8::new(2).unwrap()]).unwrap(),
            Some("serial".into()),
        )
        .unwrap();
        let mut model = ProductModel::new(submitted.clone());
        let frozen = model.draft().clone();
        let (apply, _) = model
            .apply(model.state_identity(), frozen.revision)
            .unwrap();
        session.admit_user_apply(
            ApplyAdmission::Open { apply },
            frozen,
            ProfileBinding::Serial,
        );
        let original_request = model.validation_request().unwrap().clone();
        if edit_during_apply {
            model
                .edit_draft(model.draft().revision, selection(30000))
                .unwrap();
        }
        // Real domain preparation cutover mirrors the capture validator's fresh
        // serial-preserving topology normalization, without replacing the draft.
        let request = model
            .accept_prepared(
                &original_request,
                normalized.clone(),
                original_request.watch,
                None,
            )
            .unwrap();
        let Some(ModelEffect::Open { key, .. }) = model.fixture_validation_succeeded(&request)
        else {
            panic!("opening")
        };
        session.observe_opening(key);
        verify_open(&mut model, key);
        session.verified_applied(VerifiedApplied::Open {
            key,
            applied: model.last_valid().unwrap().clone(),
        });
        let document = load_document(temp.file());
        assert_eq!(document.applied, Some(normalized));
        assert_eq!(
            model.draft().settings.video.mode,
            selection(if edit_during_apply { 30000 } else { 60000 })
                .video
                .mode
        );
        assert!(session.request_close());
        assert_eq!(session.close_dialog(), "");
        session.drained();
        assert_eq!(load_document(temp.file()).applied, document.applied);
    }

    #[test]
    fn relocated_verified_identity_saves_fresh_topology_without_saving_candidate_on_close() {
        qualify_relocated_user_apply(false);
    }
    #[test]
    fn relocated_verified_identity_never_saves_edits_during_apply_or_on_close() {
        qualify_relocated_user_apply(true);
    }
    fn other_source() -> DraftSettings {
        let mut settings = selection(30000);
        settings.video.identity = DeviceIdentity::new(
            0x1234,
            0xabcd,
            UsbTopology::new("pci-test".into(), vec![NonZeroU8::new(2).unwrap()]).unwrap(),
            Some("source-b".into()),
        )
        .unwrap();
        settings.audio = AudioSelection::Disabled {
            retained: Some(source("saved-b")),
        };
        settings.renderer = reference_renderer();
        settings
    }

    #[test]
    fn failed_save_a_then_success_b_and_close_retry_keep_both_verified_profiles() {
        use crate::settings::WriteFaultPoint;
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        session.store = Some(SettingsStore::with_injected_fault(
            temp.file(),
            WriteFaultPoint::BeforeRename,
        ));
        let a = selection(60000);
        let b = other_source();
        user_apply(&mut session, a.clone());
        assert!(!temp.file().exists());
        assert!(session.store.as_ref().unwrap().snapshot().is_none());
        assert_eq!(session.source_profile(&a.video.identity, true), Some(&a));
        user_apply(&mut session, b.clone());
        let restored = PersistenceSession::load(Ok(temp.file()));
        assert_eq!(restored.source_profile(&a.video.identity, true), Some(&a));
        assert_eq!(restored.source_profile(&b.video.identity, true), Some(&b));
        assert_eq!(restored.saved_selection(), Some(b.clone()));
        session.store = Some(SettingsStore::with_injected_fault(
            temp.file(),
            WriteFaultPoint::BeforeRename,
        ));
        session.set_preferences(preferences(0));
        session.request_close();
        session.drained();
        assert!(!session.quit_allowed());
        session.retry_close_save();
        assert!(session.quit_allowed());
        let restored = PersistenceSession::load(Ok(temp.file()));
        assert_eq!(restored.source_profile(&a.video.identity, true), Some(&a));
        assert_eq!(restored.source_profile(&b.video.identity, true), Some(&b));
        assert_eq!(restored.preferences(), &preferences(0));
    }

    #[test]
    fn adapted_startup_only_saves_correlated_verified_receipt_and_reset_revokes_it() {
        for reset in [false, true] {
            let temp = Temp::new();
            let mut session = PersistenceSession::load(Ok(temp.file()));
            user_apply(&mut session, selection(60000));
            let before = fs::read(temp.file()).unwrap();
            let adapted = selection(30000);
            let receipt = event(adapted.clone());
            let VerifiedApplied::Open { key, .. } = &receipt else {
                panic!("open proof")
            };
            session.admit_startup_adaptation(
                ApplyAdmission::Open { apply: key.apply },
                Draft {
                    revision: DraftRevision::new(0),
                    settings: adapted.clone(),
                },
                ProfileBinding::Serial,
                StartupAudioPolicy::Explicit,
            );
            assert_eq!(fs::read(temp.file()).unwrap(), before);
            session.observe_opening(*key);
            if reset {
                session.request_reset();
                session.decide_reset(session.reset_token(), true);
            }
            session.verified_applied(receipt);
            assert_eq!(session.saved_selection(), (!reset).then_some(adapted));
            session.request_close();
            session.drained();
            assert_eq!(
                PersistenceSession::load(Ok(temp.file())).saved_selection(),
                session.saved_selection()
            );
        }
    }

    #[test]
    fn cli_startup_adaptation_captures_historical_audio_and_still_checks_actual_receipt() {
        for reset in [false, true] {
            let temp = Temp::new();
            let mut session = PersistenceSession::load(Ok(temp.file()));
            let mut original = selection(60000);
            original.audio = AudioSelection::Enabled {
                source: source("saved-a"),
            };
            user_apply(&mut session, original.clone());
            let mut submitted = selection(30000);
            submitted.audio = AudioSelection::Enabled {
                source: source("cli-b"),
            };
            let receipt = event(submitted.clone());
            let VerifiedApplied::Open { key, .. } = &receipt else {
                panic!("open")
            };
            let key = *key;
            session.admit_startup_adaptation(
                ApplyAdmission::Open { apply: key.apply },
                Draft {
                    revision: DraftRevision::new(0),
                    settings: submitted.clone(),
                },
                ProfileBinding::Serial,
                StartupAudioPolicy::Cli,
            );
            session.observe_opening(key);
            // A later working-profile mutation cannot change the admitted
            // historical audio authority, nor authorize a different receipt.
            let mut later = submitted.clone();
            later.audio = AudioSelection::Disabled {
                retained: Some(source("later")),
            };
            let VerifiedApplied::Open { applied, .. } = event(later.clone()) else {
                panic!("open")
            };
            session
                .profiles
                .upsert_verified(&applied, ProfileBinding::Serial);
            session.verified_applied(VerifiedApplied::Open { key, applied });
            assert_eq!(session.authorized_apply(), Some(key.apply));
            assert_eq!(load_document(temp.file()).applied, Some(original.clone()));
            if reset {
                session.request_reset();
                session.decide_reset(session.reset_token(), true);
            }
            session.verified_applied(receipt);
            let mut projected = submitted;
            projected.audio = original.audio;
            assert_eq!(
                session.saved_selection(),
                (!reset).then_some(projected.clone())
            );
            if !reset {
                assert_eq!(load_document(temp.file()).applied, Some(projected));
                // A subsequent normal user transaction persists its real audio.
                user_apply(&mut session, later.clone());
                assert_eq!(load_document(temp.file()).applied, Some(later));
            }
        }
    }

    #[test]
    fn cli_startup_adaptation_without_historical_profile_cannot_create_one() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        let submitted = selection(30000);
        let receipt = event(submitted.clone());
        let VerifiedApplied::Open { key, .. } = &receipt else {
            panic!("open")
        };
        let key = *key;
        session.admit_startup_adaptation(
            ApplyAdmission::Open { apply: key.apply },
            Draft {
                revision: DraftRevision::new(0),
                settings: submitted,
            },
            ProfileBinding::Serial,
            StartupAudioPolicy::Cli,
        );
        assert_eq!(session.authorized_apply(), None);
        session.observe_opening(key);
        session.verified_applied(receipt);
        assert!(session.saved_selection().is_none());
        assert!(!temp.file().exists());
    }

    #[test]
    fn startup_adapts_only_typed_video_incompatibility_and_never_overrides_audio_failure() {
        let mut saved = selection(60000);
        let adapted = decide_startup(
            Some(saved.clone()),
            None,
            |_| {
                Err(StartupVideoError::Incompatible(
                    "size no longer advertised".into(),
                ))
            },
            Ok(&[]),
        );
        assert!(adapted.adapt_video);
        assert!(!adapted.auto_open);
        assert_eq!(adapted.draft, Some(saved.clone()));
        let unknown = decide_startup(
            Some(saved.clone()),
            None,
            |_| Err(StartupVideoError::Unavailable("ioctl unavailable".into())),
            Ok(&[]),
        );
        assert!(!unknown.adapt_video);
        saved.audio = AudioSelection::Enabled {
            source: source("missing"),
        };
        let missing_audio = decide_startup(
            Some(saved.clone()),
            None,
            |_| Err(StartupVideoError::Incompatible("unsupported mode".into())),
            Ok(&[]),
        );
        assert!(!missing_audio.adapt_video);
        assert!(!missing_audio.auto_open);
        assert_eq!(missing_audio.draft, Some(saved));
        assert!(missing_audio.reason.contains("audio"));
    }

    #[test]
    fn topology_admission_cannot_save_an_unrelated_or_relocated_identity() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        let submitted = selection(60000);
        let mut wrong = submitted.clone();
        wrong.video.identity = other_source().video.identity;
        let receipt = event(wrong);
        let VerifiedApplied::Open { key, .. } = &receipt else {
            panic!("open proof")
        };
        session.admit_user_apply(
            ApplyAdmission::Open { apply: key.apply },
            Draft {
                revision: DraftRevision::new(0),
                settings: submitted,
            },
            ProfileBinding::Topology,
        );
        session.observe_opening(*key);
        session.verified_applied(receipt);
        assert!(session.saved_selection().is_none());
        assert!(!temp.file().exists());
    }
    #[test]
    fn migrated_v1_last_source_is_restored_without_write_until_authorized_close() {
        let temp = Temp::new();
        let saved = selection(60000);
        let mut original_session = PersistenceSession::load(Ok(temp.file()));
        user_apply(&mut original_session, saved.clone());
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.file()).unwrap()).unwrap();
        let profile = document["profiles"][0]["settings"].take();
        let object = document.as_object_mut().unwrap();
        object.remove("profiles");
        object.remove("last_source");
        object.insert("schema_version".into(), serde_json::json!(1));
        object.insert("applied".into(), profile);
        let v1 = serde_json::to_vec(&document).unwrap();
        fs::write(temp.file(), &v1).unwrap();
        let mut migrated = PersistenceSession::load(Ok(temp.file()));
        assert_eq!(migrated.saved_selection(), Some(saved.clone()));
        assert_eq!(migrated.saved_binding(), Some(ProfileBinding::Serial));
        assert_eq!(
            migrated.source_profile(&saved.video.identity, true),
            Some(&saved)
        );
        assert_eq!(fs::read(temp.file()).unwrap(), v1);
        let absent = decide_startup(
            migrated.saved_selection(),
            None,
            |_| Err("last source absent".into()),
            Ok(&[]),
        );
        migrated.prepare_startup(&absent);
        assert!(!absent.auto_open && !absent.adapt_video);
        assert_eq!(absent.draft, Some(saved.clone()));
        assert_eq!(fs::read(temp.file()).unwrap(), v1);
        migrated.request_close();
        migrated.drained();
        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.file()).unwrap()).unwrap();
        assert_eq!(written["schema_version"], 3);
        assert!(written.get("applied").is_none());
        assert_eq!(
            PersistenceSession::load(Ok(temp.file())).saved_selection(),
            Some(saved)
        );
    }
}
