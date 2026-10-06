//! Application persistence policy: only correlated user Apply receipts authorize
//! capture writes; draft content is solely a close-negotiation baseline.
use std::{ffi::OsStr, path::PathBuf};

use crate::{
    app::apply::VerifiedOpen,
    domain::{
        capture::AudioSourceIdentity,
        state::{
            AppliedSettings, ApplyId, AttemptKey, AttemptPurpose, Draft, DraftRevision,
            DraftSettings,
        },
    },
    settings::{Durability, LoadOutcome, LocalPreferences, SettingsStore, WriteOutcome},
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

#[derive(Debug, PartialEq, Eq)]
pub struct StartupSelection {
    pub draft: Option<DraftSettings>,
    pub auto_open: bool,
    pub reason: String,
}

/// Discovery is not atomic. Normal opening revalidates, and its initial audio
/// receipt is additionally strict. Disabled retained audio never blocks launch.
pub fn decide_startup(
    saved: Option<DraftSettings>,
    explicit: Option<DraftSettings>,
    check_video: impl FnOnce(&DraftSettings) -> Result<(), String>,
    audio: Result<&[AudioSourceIdentity], String>,
) -> StartupSelection {
    if let Some(draft) = explicit {
        return StartupSelection { draft: Some(draft), auto_open: false, reason: "Explicit command-line selection; saved capture auto-open suspended until a user Apply.".into() };
    }
    let Some(draft) = saved else {
        return StartupSelection {
            draft: None,
            auto_open: false,
            reason:
                "No saved capture selection. Local preferences remain available without a stream."
                    .into(),
        };
    };
    let result = check_video(&draft).and_then(|()| {
        if !draft.audio.enabled() { return Ok(()); }
        let sources = audio?;
        let source = draft.audio.source().ok_or("Saved enabled audio has no source.")?;
        let mut matches = sources.iter().filter(|observed| source.compatible_with(observed));
        match (matches.next(), matches.next()) {
            (Some(_), None) => Ok(()),
            (None, _) => Err(format!("Saved enabled audio source {:?} is absent or its stable identity differs. No stream opened.", source.name())),
            _ => Err(format!("Saved enabled audio source {:?} is ambiguous. No stream opened.", source.name())),
        }
    });
    match result {
        Ok(()) => StartupSelection {
            draft: Some(draft),
            auto_open: true,
            reason: "Saved sources and exact capture tuple are available; restoring live capture."
                .into(),
        },
        Err(reason) => StartupSelection {
            draft: Some(draft),
            auto_open: false,
            reason: format!("Saved selection pre-filled, not active: {reason}"),
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CloseState {
    Open,
    ConfirmDraft {
        revision: DraftRevision,
        settings: DraftSettings,
    },
    Draining,
    SaveFailed,
    DurabilityWarning,
    Allowed,
}

pub struct PersistenceSession {
    store: Option<SettingsStore>,
    preferences: LocalPreferences,
    baseline: Option<DraftSettings>,
    user_apply: Option<(ApplyId, Draft)>,
    user_open: Option<AttemptKey>,
    pending_applied: Option<AppliedSettings>,
    close: CloseState,
    reset_token: Option<u64>,
    next_reset_token: u64,
    load_status: String,
    write_status: String,
    durability_unconfirmed: bool,
    startup_reason: String,
}
impl PersistenceSession {
    pub fn load(path: Result<PathBuf, String>) -> Self {
        let mut session = Self {
            store: None,
            preferences: LocalPreferences::default(),
            baseline: None,
            user_apply: None,
            user_open: None,
            pending_applied: None,
            close: CloseState::Open,
            reset_token: None,
            next_reset_token: 0,
            load_status: String::new(),
            write_status: String::new(),
            startup_reason: String::new(),
            durability_unconfirmed: false,
        };
        match path {
            Err(error) => session.load_status = error,
            Ok(path) => {
                let (store, outcome) = SettingsStore::load(path);
                session.load_status = match outcome {
                    LoadOutcome::Missing => "First-use session preferences (not restored); they save at safe application close.".into(),
                    LoadOutcome::Loaded(document) => {
                        session.preferences = document.preferences;
                        session.baseline = document.applied;
                        "Saved local preferences restored.".into()
                    }
                    LoadOutcome::Refused(error) => format!("Settings file refused: {error}. Original preserved. ALL automatic saves are blocked; session preference changes will NOT survive relaunch until explicit reset. Current values are initial session values, not restored."),
                };
                session.store = Some(store);
            }
        }
        session
    }
    pub fn preferences(&self) -> LocalPreferences {
        self.preferences
    }
    pub fn set_preferences(&mut self, preferences: LocalPreferences) {
        self.preferences = preferences;
    }
    pub fn saved_selection(&self) -> Option<DraftSettings> {
        self.store
            .as_ref()
            .and_then(|store| store.snapshot())
            .and_then(|document| document.applied.clone())
    }
    pub fn prepare_startup(&mut self, selection: &StartupSelection) {
        self.baseline = selection.draft.clone();
        self.startup_reason = selection.reason.clone();
    }
    pub fn startup_failed(&mut self, reason: String) {
        self.startup_reason =
            format!("Saved selection pre-filled, not active: initial restoration failed: {reason}");
    }
    pub fn startup_verified(&mut self) {
        self.startup_reason = "Startup restoration completed: exact capture and initial audio receipt verified. Pause state was not restored.".into();
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
    pub fn draft_dirty(&self, draft: Option<&DraftSettings>) -> bool {
        draft != self.baseline.as_ref()
    }
    pub fn admit_user_apply(&mut self, apply: ApplyId, submitted: Draft) {
        self.user_apply = Some((apply, submitted));
        self.user_open = None;
    }
    pub fn observe_opening(&mut self, key: AttemptKey) {
        if self
            .user_apply
            .as_ref()
            .is_some_and(|(apply, _)| *apply == key.apply)
            && key.purpose == AttemptPurpose::Candidate
        {
            self.user_open = Some(key);
        }
    }
    pub fn verified_open(&mut self, event: VerifiedOpen) {
        if self.user_open != Some(event.key)
            || self
                .user_apply
                .as_ref()
                .is_none_or(|(apply, _)| *apply != event.key.apply)
        {
            return;
        }
        let Some((_, submitted)) = self.user_apply.take() else {
            return;
        };
        self.user_open = None;
        // Fresh validation may normalize a unique serial device's topology.
        // Cleanliness describes the submitted user selection, not that fresh
        // receipt; a newer draft edit must remain different from this baseline.
        self.baseline = Some(submitted.settings);
        self.pending_applied = Some(event.applied);
        self.save();
    }
    fn save(&mut self) -> bool {
        let Some(store) = &mut self.store else {
            self.write_status = "Save unavailable: no valid configuration location. Session preferences are not durable.".into();
            return false;
        };
        let outcome = match &self.pending_applied {
            Some(applied) => store.save_applied(applied, self.preferences),
            None => store.save_preferences(self.preferences),
        };
        match outcome {
            Err(error) => {
                self.write_status = format!(
                    "Save failed at {}: {error}. Previous published file preserved.",
                    store.path().display()
                );
                false
            }
            Ok(WriteOutcome::Committed { durability }) => {
                self.pending_applied = None;
                self.durability_unconfirmed = matches!(&durability, Durability::Unconfirmed(_));
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
        matches!(
            self.close,
            CloseState::Open | CloseState::ConfirmDraft { .. }
        )
    }
    /// Returns true exactly when the native owner should begin normal teardown.
    pub fn request_close(&mut self, draft: Option<(&DraftSettings, DraftRevision)>) -> bool {
        if !matches!(self.close, CloseState::Open) {
            return false;
        }
        self.reset_token = None;
        if let Some((settings, revision)) =
            draft.filter(|(settings, _)| self.draft_dirty(Some(settings)))
        {
            self.close = CloseState::ConfirmDraft {
                revision,
                settings: settings.clone(),
            };
            false
        } else {
            self.close = CloseState::Draining;
            true
        }
    }
    pub fn decide_dirty_close(
        &mut self,
        discard: bool,
        revision: DraftRevision,
        draft: Option<(&DraftSettings, DraftRevision)>,
    ) -> bool {
        let CloseState::ConfirmDraft {
            revision: expected,
            settings,
        } = &self.close
        else {
            return false;
        };
        if !discard {
            self.close = CloseState::Open;
            return false;
        }
        let Some((current, current_revision)) = draft else {
            return false;
        };
        if revision != *expected || current_revision != *expected || current != settings {
            self.close = CloseState::ConfirmDraft {
                revision: current_revision,
                settings: current.clone(),
            };
            return false;
        }
        self.close = CloseState::Draining;
        true
    }
    pub fn close_revision(&self) -> u64 {
        match self.close {
            CloseState::ConfirmDraft { revision, .. } => revision.get(),
            _ => 0,
        }
    }
    pub fn close_dialog(&self) -> &'static str {
        match self.close {
            CloseState::ConfirmDraft { .. } => "draft",
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
        self.close = CloseState::Allowed;
    }
    pub fn request_reset(&mut self) {
        if self.close != CloseState::Open || self.store.is_none() || self.reset_token.is_some() {
            return;
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
        let Some(store) = &mut self.store else {
            return;
        };
        match store.reset(self.preferences) {
            Err(error) => {
                self.write_status = format!(
                    "Reset failed at {}: {error}. Original and refusal preserved.",
                    store.path().display()
                )
            }
            Ok(WriteOutcome::Committed { durability }) => {
                // An older in-flight Apply or retry cannot resurrect cleared saved capture.
                self.pending_applied = None;
                self.user_apply = None;
                self.user_open = None;
                self.load_status = "Saved file explicitly reset; capture is not saved. Live capture and draft unchanged.".into();
                self.write_status = match durability {
                    Durability::Confirmed => {
                        "Reset committed with current session preferences and applied:null.".into()
                    }
                    Durability::Unconfirmed(error) => format!(
                        "Reset committed (applied:null), but crash durability is NOT confirmed: {error}"
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
        state::{ModelEffect, ProductModel},
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
            video: ModeRequest {
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
            },
            audio: AudioSelection::default(),
        }
    }
    fn source(serial: &str) -> AudioSourceIdentity {
        AudioSourceIdentity::new(
            "capture-source".into(),
            vec![("device.serial".into(), serial.into())],
        )
        .unwrap()
    }
    fn event(settings: DraftSettings) -> VerifiedOpen {
        let mut model = ProductModel::new(settings);
        model
            .apply(model.state_identity(), model.draft().revision)
            .unwrap();
        let request = model.validation_request().unwrap().clone();
        let Some(ModelEffect::Open { key, .. }) = model.validation_succeeded(&request) else {
            panic!("opening")
        };
        model.open_verified(key);
        VerifiedOpen {
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
        session.admit_user_apply(event.key.apply, submitted);
        session.observe_opening(event.key);
        session.verified_open(event);
    }
    fn load_document(path: PathBuf) -> crate::settings::StoredDocument {
        let (_, LoadOutcome::Loaded(document)) = SettingsStore::load(path) else {
            panic!("document not loadable")
        };
        document
    }
    fn preferences(percent: u8) -> LocalPreferences {
        LocalPreferences {
            gain: PlaybackGain::new(percent, true).unwrap(),
            fullscreen: true,
        }
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
    fn only_exact_correlated_user_receipt_saves_once_not_startup_restart_or_rollback() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        session.verified_open(event(selection(60000)));
        assert!(!temp.file().exists());
        let mut receipt = event(selection(60000));
        session.admit_user_apply(
            receipt.key.apply,
            Draft {
                revision: DraftRevision::new(0),
                settings: selection(60000),
            },
        );
        let wrong_key = AttemptKey {
            attempt: crate::domain::state::AttemptId::new(receipt.key.attempt.get() + 1).unwrap(),
            ..receipt.key
        };
        session.observe_opening(wrong_key);
        session.verified_open(event(selection(60000)));
        assert!(!temp.file().exists());
        receipt.key.purpose = AttemptPurpose::Restore;
        session.verified_open(receipt);
        assert!(!temp.file().exists());
        user_apply(&mut session, selection(60000));
        assert_eq!(load_document(temp.file()).applied, Some(selection(60000)));
        let bytes = fs::read(temp.file()).unwrap();
        session.verified_open(event(selection(30000)));
        assert_eq!(fs::read(temp.file()).unwrap(), bytes);
    }
    #[test]
    fn dirty_is_content_equality_and_preferences_never_dirty_selection() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        let original = selection(60000);
        session.prepare_startup(&StartupSelection {
            draft: Some(original.clone()),
            auto_open: false,
            reason: String::new(),
        });
        assert!(!session.draft_dirty(Some(&original)));
        assert!(session.draft_dirty(Some(&selection(30000))));
        session.set_preferences(preferences(17));
        assert!(!session.draft_dirty(Some(&original)));
    }
    #[test]
    fn close_cancel_never_drains_and_stale_discard_requires_new_decision() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        user_apply(&mut session, selection(60000));
        let draft = selection(30000);
        assert!(!session.request_close(Some((&draft, DraftRevision::new(1)))));
        assert!(!session.request_close(Some((&draft, DraftRevision::new(1)))));
        assert!(!session.decide_dirty_close(
            false,
            DraftRevision::new(1),
            Some((&draft, DraftRevision::new(1)))
        ));
        assert!(session.mutations_open());
        assert_eq!(session.close_dialog(), "");
        session.request_close(Some((&draft, DraftRevision::new(1))));
        let latest = selection(24000);
        assert!(!session.decide_dirty_close(
            true,
            DraftRevision::new(1),
            Some((&latest, DraftRevision::new(2)))
        ));
        assert!(session.mutations_open());
        assert_eq!(session.close_revision(), 2);
        assert!(session.decide_dirty_close(
            true,
            DraftRevision::new(2),
            Some((&latest, DraftRevision::new(2)))
        ));
        assert!(!session.mutations_open());
    }
    #[test]
    fn discard_saves_applied_not_draft_and_latest_preferences_after_drain() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        user_apply(&mut session, selection(60000));
        let draft = selection(30000);
        session.set_preferences(preferences(23));
        session.request_close(Some((&draft, DraftRevision::new(1))));
        assert!(session.decide_dirty_close(
            true,
            DraftRevision::new(1),
            Some((&draft, DraftRevision::new(1)))
        ));
        assert!(!session.quit_allowed());
        session.drained();
        assert!(session.quit_allowed());
        let document = load_document(temp.file());
        assert_eq!(document.applied, Some(selection(60000)));
        assert_eq!(document.preferences, preferences(23));
    }
    #[test]
    fn preference_only_close_without_capture_restores_latest_preferences_and_null_capture() {
        let temp = Temp::new();
        let mut session = PersistenceSession::load(Ok(temp.file()));
        session.set_preferences(preferences(41));
        assert!(session.request_close(None));
        session.drained();
        assert!(session.quit_allowed());
        let reloaded = PersistenceSession::load(Ok(temp.file()));
        assert_eq!(reloaded.preferences(), preferences(41));
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
            assert!(session.request_close(Some((&selection(60000), DraftRevision::new(0)))));
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
        let baseline = session.baseline.clone();
        session.set_preferences(preferences(12));
        session.request_reset();
        let token = session.reset_token();
        session.decide_reset(token + 1, true);
        assert!(session.refused());
        session.decide_reset(token, true);
        assert!(!session.refused());
        assert_eq!(session.baseline, baseline);
        assert_eq!(session.preferences(), preferences(12));
        assert_eq!(load_document(temp.file()).applied, None);
        session.decide_reset(token, true);
        session.request_close(Some((&selection(60000), DraftRevision::new(0))));
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
        assert!(session.pending_applied.is_some());
        assert!(session.status().contains("Save failed"));
        session.set_preferences(preferences(9));
        assert!(session.request_close(Some((&selection(60000), DraftRevision::new(0)))));
        session.drained();
        assert_eq!(session.close_dialog(), "save");
        fs::remove_file(&blocked).unwrap();
        session.retry_close_save();
        assert!(session.quit_allowed());
        let document = load_document(path);
        assert_eq!(document.applied, Some(selection(60000)));
        assert_eq!(document.preferences, preferences(9));
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
        session.admit_user_apply(apply, frozen);
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
            )
            .unwrap();
        let Some(ModelEffect::Open { key, .. }) = model.validation_succeeded(&request) else {
            panic!("opening")
        };
        session.observe_opening(key);
        model.open_verified(key);
        session.verified_open(VerifiedOpen {
            key,
            applied: model.last_valid().unwrap().clone(),
        });
        let document = load_document(temp.file());
        assert_eq!(document.applied, Some(normalized));
        assert_eq!(session.baseline, Some(submitted));
        assert_eq!(
            session.draft_dirty(Some(&model.draft().settings)),
            edit_during_apply
        );
        let close = session.request_close(Some((&model.draft().settings, model.draft().revision)));
        assert_eq!(close, !edit_during_apply);
        assert_eq!(
            session.close_dialog(),
            if edit_during_apply { "draft" } else { "" }
        );
    }

    #[test]
    fn relocated_verified_identity_saves_fresh_topology_but_unedited_submitted_selection_is_clean()
    {
        qualify_relocated_user_apply(false);
    }
    #[test]
    fn relocated_verified_identity_never_marks_edits_during_apply_clean_or_persists_them() {
        qualify_relocated_user_apply(true);
    }
}
