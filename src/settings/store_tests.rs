//! FUR-012 settings store behavior regressions: every load refusal class,
//! exact-FPS roundtrip, atomic-writer fault survival, explicit reset and
//! post-commit durability uncertainty.

use std::fs;
use std::num::NonZeroU8;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::domain::capture::{
    AudioSelection, AudioSourceIdentity, CaptureModeRequest, CapturedFourCc, DeviceIdentity,
    FrameRate, FrameSize, InputFormatPolicy, PlaybackGain, UsbTopology, VideoRequest,
};
use crate::domain::output::{PersistentOutputChoice, SinkIdentity};
use crate::domain::render::{
    DebandValues, DitherMode, RendererSettings, RendererValues, ScalerChoice, ScalerKernel,
};
use crate::domain::state::{AppliedSettings, DraftSettings, ModelEffect, ProductModel};
use crate::settings::{
    Durability, LoadOutcome, LocalPreferences, ProfileBinding, ProfileValidationError,
    SettingsLoadError, SettingsStore, SettingsValidationError, SettingsWriteError, SourceProfile,
    SourceProfiles, StoredDocument, WriteFaultPoint, WriteOutcome,
};

// ---------------------------------------------------------------------------
// Isolated temp directories (no new dependencies)
// ---------------------------------------------------------------------------

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let unique = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "furami-settings-{}-{tag}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn settings_path(directory: &Path) -> PathBuf {
    directory.join("settings.json")
}

fn assert_no_temp_files(directory: &Path) {
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        assert!(
            !entry.file_name().to_string_lossy().contains("furami-tmp"),
            "temporary file residue: {:?}",
            entry.file_name()
        );
    }
}

// ---------------------------------------------------------------------------
// Domain fixtures (built through the validating constructors)
// ---------------------------------------------------------------------------

fn topology() -> UsbTopology {
    UsbTopology::new(
        "usb-0000:03:00.0".to_owned(),
        vec![NonZeroU8::new(2).unwrap(), NonZeroU8::new(1).unwrap()],
    )
    .unwrap()
}

fn identity() -> DeviceIdentity {
    DeviceIdentity::new(0x1234, 0x5678, topology(), Some("SER42".to_owned())).unwrap()
}

/// A manual-policy video request: the pin and the proven concrete format agree,
/// which is the shape both legacy v1 documents and prepared applies produce.
fn video_request(
    fourcc: u32,
    width: u32,
    height: u32,
    numerator: u32,
    denominator: u32,
) -> VideoRequest {
    let concrete = CapturedFourCc::from_kernel(fourcc);
    VideoRequest {
        identity: identity(),
        mode: CaptureModeRequest {
            format_policy: InputFormatPolicy::Manual { fourcc: concrete },
            size: FrameSize::new(width, height).unwrap(),
            rate: FrameRate::new(numerator, denominator).unwrap(),
        },
        resolved_fourcc: Some(concrete),
        selected_node: None,
    }
}

fn source_identity() -> AudioSourceIdentity {
    AudioSourceIdentity::new(
        "alsa_input.usb-ShadowCast".to_owned(),
        vec![
            ("card.id".to_owned(), "3".to_owned()),
            ("node.name".to_owned(), "capture.hdmi".to_owned()),
        ],
    )
    .unwrap()
}

fn draft_settings() -> DraftSettings {
    DraftSettings {
        video: video_request(0x3231_564E, 1920, 1080, 60000, 1001),
        audio: AudioSelection::Enabled {
            source: source_identity(),
        },
        renderer: crate::domain::render::RendererSettings::default(),
    }
}

fn preferences() -> LocalPreferences {
    LocalPreferences {
        gain: PlaybackGain::new(73, false).unwrap(),
        fullscreen: true,
        output: PersistentOutputChoice::Auto,
        stats_hidden: false,
        logging_enabled: false,
    }
}

/// Consumer fixtures always save a complete collection through the new API.
fn last_settings(document: &StoredDocument) -> Option<&DraftSettings> {
    let identity = document.last_source.as_ref()?;
    document
        .profiles
        .entries()
        .iter()
        .find(|profile| &profile.settings().video.identity == identity)
        .map(SourceProfile::settings)
}

fn persist_verified(
    store: &mut SettingsStore,
    applied: &AppliedSettings,
    preferences: LocalPreferences,
) -> Result<WriteOutcome, SettingsWriteError> {
    let mut profiles = store
        .snapshot()
        .map(|document| document.profiles.clone())
        .unwrap_or_default();
    let identity = &applied.settings().video.identity;
    let binding = profiles.binding_for(identity, true);
    profiles.upsert_verified(applied, binding);
    store.save_profiles(&profiles, Some(identity), preferences)
}

fn persist_globals(
    store: &mut SettingsStore,
    preferences: LocalPreferences,
) -> Result<WriteOutcome, SettingsWriteError> {
    let profiles = store
        .snapshot()
        .map(|document| document.profiles.clone())
        .unwrap_or_default();
    let last_source = store
        .snapshot()
        .and_then(|document| document.last_source.clone());
    store.save_profiles(&profiles, last_source.as_ref(), preferences)
}

/// Mints an `AppliedSettings` through the only legitimate producer: a full
/// verified open on the product model. The aggregate is keyed to the model's
/// actual opening request and built with the pub(crate) checked fixture proof.
///
/// The model freezes a resolution-less candidate; the prepared settings are
/// supplied through `accept_prepared` so the persisted aggregate carries the
/// proven concrete format, exactly as the production adapter does.
fn applied_settings(settings: DraftSettings) -> AppliedSettings {
    let mut model = ProductModel::new(settings.clone());
    model
        .apply(model.state_identity(), model.draft().revision)
        .unwrap();
    let request = model.validation_request().unwrap().clone();
    let prepared = model
        .accept_prepared(&request, settings, request.watch, None)
        .expect("model accepts the resolved prepared settings");
    let Some(ModelEffect::Open { key, .. }) = model.validation_succeeded(&prepared) else {
        panic!("initial open effect missing");
    };
    open_verified(&mut model, key);
    model.last_valid().unwrap().clone()
}

/// Open aggregate keyed to the physical opening: the media owner returns it
/// only after nine native renderer readbacks and matching playback.
fn open_verified(model: &mut ProductModel, key: crate::domain::state::AttemptKey) {
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

// ---------------------------------------------------------------------------
// JSON fixtures (wire format pinned literally, not produced by the writer)
// ---------------------------------------------------------------------------

const APPLIED: &str = r#"{"video":{"identity":{"vendor_id":4660,"product_id":22136,"topology":{"controller":"usb-0000:03:00.0","ports":[2,1]},"serial":"SER42"},"mode":{"captured_fourcc":842094158,"width":1920,"height":1080,"fps":{"numerator":60000,"denominator":1001}}},"audio":{"kind":"enabled","source":{"name":"alsa_input.usb-ShadowCast","stable_properties":[{"key":"card.id","value":"3"},{"key":"node.name","value":"capture.hdmi"}]}}}"#;

const PREFS: &str = r#"{"volume_percent":73,"muted":false,"fullscreen":true}"#;

const SOURCE_FRAGMENT: &str = r#""source":{"name":"alsa_input.usb-ShadowCast","stable_properties":[{"key":"card.id","value":"3"},{"key":"node.name","value":"capture.hdmi"}]}"#;

fn canonical_document() -> String {
    format!(r#"{{"schema_version":1,"applied":{APPLIED},"preferences":{PREFS}}}"#)
}

fn document_with_applied(applied: &str) -> String {
    format!(r#"{{"schema_version":1,"applied":{applied},"preferences":{PREFS}}}"#)
}

fn refused_schema(directory: &Path, label: &str, json: &str) -> (SettingsStore, LoadOutcome) {
    let path = settings_path(directory);
    fs::write(&path, json).unwrap();
    let (store, outcome) = SettingsStore::load(path);
    assert!(
        matches!(outcome, LoadOutcome::Refused(_)),
        "{label}: expected refusal, got {outcome:?}"
    );
    assert!(store.is_refused(), "{label}");
    assert!(store.snapshot().is_none(), "{label}");
    (store, outcome)
}

// ---------------------------------------------------------------------------
// Load outcomes
// ---------------------------------------------------------------------------

#[test]
fn missing_file_loads_missing_without_refusal() {
    let directory = temp_dir("missing");
    let (store, outcome) = SettingsStore::load(settings_path(&directory));
    assert!(matches!(outcome, LoadOutcome::Missing));
    assert!(!store.is_refused());
    assert!(store.snapshot().is_none());
    assert_eq!(store.path(), settings_path(&directory));
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn valid_document_loads_completely() {
    let directory = temp_dir("loaded");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document()).unwrap();

    let (store, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("expected loaded document, got {outcome:?}");
    };
    assert!(!store.is_refused());
    assert_eq!(last_settings(&document), Some(&draft_settings()));
    assert_eq!(document.preferences, preferences());
    assert_eq!(store.snapshot(), Some(&*document));
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn required_nullable_fields_distinguish_null_from_missing() {
    let directory = temp_dir("nullable");
    let path = settings_path(&directory);

    // applied: null is explicit and valid.
    fs::write(&path, document_with_applied("null")).unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("applied:null must load, got {outcome:?}");
    };
    assert!(document.profiles.entries().is_empty());
    assert!(document.last_source.is_none());

    // serial: null is valid (absent serial), missing the key entirely is a
    // schema violation.
    fs::write(
        &path,
        document_with_applied(&APPLIED.replace(r#""serial":"SER42""#, r#""serial":null"#)),
    )
    .unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("serial:null must load, got {outcome:?}");
    };
    assert_eq!(
        last_settings(&document).unwrap().video.identity.serial(),
        None
    );
    assert_eq!(
        document.profiles.entries()[0].binding(),
        ProfileBinding::Topology
    );

    fs::write(
        &path,
        document_with_applied(&APPLIED.replace(r#","serial":"SER42""#, "")),
    )
    .unwrap();
    let (store, outcome) = SettingsStore::load(path.clone());
    assert!(
        matches!(
            outcome,
            LoadOutcome::Refused(SettingsLoadError::Schema { .. })
        ),
        "missing serial key must be refused, got {outcome:?}"
    );
    assert!(store.is_refused());

    // retained: null is valid; a disabled selection without the key is not.
    let disabled_null = APPLIED.replace(
        &format!(r#""kind":"enabled",{SOURCE_FRAGMENT}"#),
        r#""kind":"disabled","retained":null"#,
    );
    fs::write(&path, document_with_applied(&disabled_null)).unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("retained:null must load, got {outcome:?}");
    };
    assert_eq!(
        last_settings(&document).unwrap().audio,
        AudioSelection::Disabled { retained: None }
    );

    fs::write(
        &path,
        document_with_applied(&APPLIED.replace(
            &format!(r#""kind":"enabled",{SOURCE_FRAGMENT}"#),
            r#""kind":"disabled""#,
        )),
    )
    .unwrap();
    let (_, outcome) = SettingsStore::load(path);
    assert!(
        matches!(
            outcome,
            LoadOutcome::Refused(SettingsLoadError::Schema { .. })
        ),
        "disabled without retained key must be refused, got {outcome:?}"
    );
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn malformed_json_is_refused_with_class_malformed() {
    let directory = temp_dir("malformed");
    let cases = [
        "not json at all",
        "",
        "{\"schema_version\":1,",  // truncated
        "{\"schema_version\":1,}", // trailing garbage in object
        "\u{feff}{\"schema_version\":1,\"applied\":null,\"preferences\":{\"volume_percent\":73,\"muted\":false,\"fullscreen\":true}}", // BOM
    ];
    for (index, json) in cases.iter().enumerate() {
        let path = settings_path(&directory);
        fs::write(&path, json).unwrap();
        let (store, outcome) = SettingsStore::load(path);
        assert!(
            matches!(
                outcome,
                LoadOutcome::Refused(SettingsLoadError::MalformedJson { .. })
            ),
            "case {index}: expected MalformedJson, got {outcome:?}"
        );
        assert!(store.is_refused(), "case {index}");
    }
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn schema_violations_are_refused_individually() {
    let directory = temp_dir("schema");
    let cases: Vec<(&str, String)> = vec![
        (
            "missing schema_version",
            format!(r#"{{"applied":null,"preferences":{PREFS}}}"#),
        ),
        (
            "missing applied",
            format!(r#"{{"schema_version":1,"preferences":{PREFS}}}"#),
        ),
        (
            "missing preferences",
            r#"{"schema_version":1,"applied":null}"#.to_owned(),
        ),
        (
            "string schema_version",
            format!(r#"{{"schema_version":"1","applied":null,"preferences":{PREFS}}}"#),
        ),
        (
            "float schema_version",
            format!(r#"{{"schema_version":1.0,"applied":null,"preferences":{PREFS}}}"#),
        ),
        (
            "duplicate schema_version",
            format!(
                r#"{{"schema_version":1,"schema_version":1,"applied":null,"preferences":{PREFS}}}"#
            ),
        ),
        (
            "duplicate applied",
            format!(
                r#"{{"schema_version":1,"applied":null,"applied":null,"preferences":{PREFS}}}"#
            ),
        ),
        (
            "unknown root field",
            format!(r#"{{"schema_version":1,"applied":null,"preferences":{PREFS},"extra":true}}"#),
        ),
        (
            "null preferences",
            r#"{"schema_version":1,"applied":null,"preferences":null}"#.to_owned(),
        ),
        (
            "unknown preference field",
            r#"{"schema_version":1,"applied":null,"preferences":{"volume_percent":73,"muted":false,"fullscreen":true,"extra":1}}"#.to_owned(),
        ),
        (
            "unknown identity field",
            document_with_applied(
                &APPLIED.replace(r#""ports":[2,1]}"#, r#""ports":[2,1],"extra":1}"#),
            ),
        ),
        (
            "unknown mode field",
            document_with_applied(&APPLIED.replace(
                r#""numerator":60000,"denominator":1001}}"#,
                r#""numerator":60000,"denominator":1001},"extra":1}"#,
            )),
        ),
        (
            "unknown source property field",
            document_with_applied(&APPLIED.replace(
                r#"{"key":"card.id","value":"3"}"#,
                r#"{"key":"card.id","value":"3","extra":1}"#,
            )),
        ),
        (
            "duplicate serial key",
            document_with_applied(&APPLIED.replace(
                r#""serial":"SER42"}"#,
                r#""serial":"SER42","serial":"SER42"}"#,
            )),
        ),
        (
            "duplicate kind key",
            document_with_applied(&APPLIED.replace(
                r#""kind":"enabled""#,
                r#""kind":"enabled","kind":"enabled""#,
            )),
        ),
        (
            "unknown audio tag",
            document_with_applied(&APPLIED.replace(r#""kind":"enabled""#, r#""kind":"paused""#)),
        ),
        (
            "enabled with retained field",
            document_with_applied(&APPLIED.replace(
                r#""kind":"enabled","source""#,
                r#""kind":"enabled","retained":null,"source""#,
            )),
        ),
        (
            "enabled without source",
            document_with_applied(&APPLIED.replace(
                &format!(r#""kind":"enabled",{SOURCE_FRAGMENT}"#),
                r#""kind":"enabled""#,
            )),
        ),
        (
            "enabled with null source",
            document_with_applied(&APPLIED.replace(
                &format!(r#""kind":"enabled",{SOURCE_FRAGMENT}"#),
                r#""kind":"enabled","source":null"#,
            )),
        ),
        (
            "disabled with source field",
            document_with_applied(&APPLIED.replace(
                &format!(r#""kind":"enabled",{SOURCE_FRAGMENT}"#),
                &format!(r#""kind":"disabled","retained":null,{SOURCE_FRAGMENT}"#),
            )),
        ),
        (
            "disabled without retained",
            document_with_applied(&APPLIED.replace(
                &format!(r#""kind":"enabled",{SOURCE_FRAGMENT}"#),
                r#""kind":"disabled""#,
            )),
        ),
        (
            "missing audio kind tag",
            document_with_applied(&APPLIED.replace(r#""kind":"enabled","source""#, r#""source""#)),
        ),
        // Valid JSON with a non-object root: the syntax pass accepts it, the
        // schema probe refuses it — a typed consumer rejection, not malformed.
        ("array root", "[1,2]".to_string()),
    ];
    for (label, json) in cases {
        refused_schema(&directory, label, &json);
        assert_no_temp_files(&directory);
    }
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn positional_struct_arrays_are_schema_refusals_and_block_all_automatic_saves() {
    let directory = temp_dir("positional-structs");
    let path = settings_path(&directory);
    let applied = applied_settings(draft_settings());
    let mut cases = vec![("root", "[2]".to_owned())];
    for (label, pointer, fields) in [
        (
            "preferences",
            "/preferences",
            &["volume_percent", "muted", "fullscreen"][..],
        ),
        ("applied", "/applied", &["video", "audio"][..]),
        ("video", "/applied/video", &["identity", "mode"][..]),
        (
            "identity",
            "/applied/video/identity",
            &["vendor_id", "product_id", "topology", "serial"][..],
        ),
        (
            "topology",
            "/applied/video/identity/topology",
            &["controller", "ports"][..],
        ),
        (
            "mode",
            "/applied/video/mode",
            &["captured_fourcc", "width", "height", "fps"][..],
        ),
        (
            "fps",
            "/applied/video/mode/fps",
            &["numerator", "denominator"][..],
        ),
        ("audio", "/applied/audio", &["kind", "source"][..]),
        (
            "audio source",
            "/applied/audio/source",
            &["name", "stable_properties"][..],
        ),
        (
            "stable property",
            "/applied/audio/source/stable_properties/0",
            &["key", "value"][..],
        ),
    ] {
        let mut document: serde_json::Value = serde_json::from_str(&canonical_document()).unwrap();
        let field = document.pointer_mut(pointer).unwrap();
        let object = field.as_object_mut().unwrap();
        let positional = fields
            .iter()
            .map(|key| object.remove(*key).unwrap())
            .collect();
        *field = serde_json::Value::Array(positional);
        cases.push((label, serde_json::to_string(&document).unwrap()));
    }

    for (label, json) in cases {
        fs::write(&path, json.as_bytes()).unwrap();
        let (mut store, outcome) = SettingsStore::load(path.clone());
        assert!(
            matches!(
                outcome,
                LoadOutcome::Refused(SettingsLoadError::Schema { .. })
            ),
            "{label}: expected schema refusal, got {outcome:?}"
        );
        assert!(store.is_refused(), "{label}");
        assert!(store.snapshot().is_none(), "{label}");
        assert!(
            matches!(
                persist_globals(&mut store, preferences()),
                Err(SettingsWriteError::RefusedOriginal { .. })
            ),
            "{label}: preference autosave must be blocked"
        );
        assert_eq!(fs::read(&path).unwrap(), json.as_bytes(), "{label}");
        assert!(
            matches!(
                persist_verified(&mut store, &applied, preferences()),
                Err(SettingsWriteError::RefusedOriginal { .. })
            ),
            "{label}: applied autosave must be blocked"
        );
        assert_eq!(fs::read(&path).unwrap(), json.as_bytes(), "{label}");
        assert!(store.snapshot().is_none(), "{label}");
        assert_no_temp_files(&directory);
    }
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn value_violations_are_refused_without_touching_the_file() {
    let directory = temp_dir("values");
    let path = settings_path(&directory);
    let cases: Vec<(&str, String, bool)> = vec![
        // (label, json, schema-level refusal instead of value-level)
        (
            "controller with slash",
            document_with_applied(&APPLIED.replace("usb-0000:03:00.0", "/")),
            false,
        ),
        (
            "empty serial string",
            document_with_applied(&APPLIED.replace("SER42", "")),
            false,
        ),
        (
            "duplicate stable property keys",
            document_with_applied(&APPLIED.replace(
                r#"[{"key":"card.id","value":"3"},{"key":"node.name","value":"capture.hdmi"}]"#,
                r#"[{"key":"k","value":"1"},{"key":"k","value":"2"}]"#,
            )),
            false,
        ),
        (
            "zero width",
            document_with_applied(&APPLIED.replace(r#""width":1920"#, r#""width":0"#)),
            false,
        ),
        (
            "zero fps numerator",
            document_with_applied(&APPLIED.replace(r#""numerator":60000"#, r#""numerator":0"#)),
            false,
        ),
        (
            "zero port",
            document_with_applied(&APPLIED.replace(r#""ports":[2,1]"#, r#""ports":[0,1]"#)),
            true,
        ),
        (
            "volume above 100",
            r#"{"schema_version":1,"applied":null,"preferences":{"volume_percent":101,"muted":false,"fullscreen":true}}"#.to_owned(),
            false,
        ),
        (
            "volume 255",
            r#"{"schema_version":1,"applied":null,"preferences":{"volume_percent":255,"muted":false,"fullscreen":true}}"#.to_owned(),
            false,
        ),
        (
            "volume 256 overflows u8",
            r#"{"schema_version":1,"applied":null,"preferences":{"volume_percent":256,"muted":false,"fullscreen":true}}"#.to_owned(),
            true,
        ),
        (
            "negative volume",
            r#"{"schema_version":1,"applied":null,"preferences":{"volume_percent":-1,"muted":false,"fullscreen":true}}"#.to_owned(),
            true,
        ),
    ];
    for (label, json, schema_level) in cases {
        fs::write(&path, &json).unwrap();
        let original = fs::read(&path).unwrap();
        let (store, outcome) = SettingsStore::load(path.clone());
        let refused_as_expected = if schema_level {
            matches!(
                outcome,
                LoadOutcome::Refused(SettingsLoadError::Schema { .. })
            )
        } else {
            matches!(
                outcome,
                LoadOutcome::Refused(SettingsLoadError::InvalidValue { .. })
            )
        };
        assert!(
            refused_as_expected,
            "{label}: unexpected outcome {outcome:?}"
        );
        assert!(store.is_refused(), "{label}");
        assert_eq!(fs::read(&path).unwrap(), original, "{label}");
    }
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn unsupported_schema_versions_are_refused_before_decoding() {
    let directory = temp_dir("versions");
    for version in ["0", "4", "999", "18446744073709551615"] {
        let json =
            format!(r#"{{"schema_version":{version},"applied":null,"preferences":{PREFS}}}"#);
        let (store, outcome) = refused_schema(&directory, version, &json);
        let LoadOutcome::Refused(SettingsLoadError::UnsupportedVersion {
            version: reported, ..
        }) = outcome
        else {
            panic!("version {version}: expected UnsupportedVersion, got {outcome:?}");
        };
        assert_eq!(reported.to_string(), version);
        assert!(store.snapshot().is_none());
    }
    // A version too large for u64 is a schema-level refusal, not a version.
    refused_schema(
        &directory,
        "overflow version",
        r#"{"schema_version":99999999999999999999,"applied":null,"preferences":{"volume_percent":73,"muted":false,"fullscreen":true}}"#,
    );
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn io_error_is_refused_and_blocks_saves() {
    let directory = temp_dir("io");
    let path = directory.join("as-directory");
    fs::create_dir(&path).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Refused(SettingsLoadError::Io { path: reported, .. }) = outcome else {
        panic!("expected io refusal, got {outcome:?}");
    };
    assert_eq!(reported, path);
    assert!(store.is_refused());
    assert!(matches!(
        persist_globals(&mut store, preferences()),
        Err(SettingsWriteError::RefusedOriginal { .. })
    ));
    fs::remove_dir_all(&directory).unwrap();
}

// ---------------------------------------------------------------------------
// Roundtrip and exact FPS
// ---------------------------------------------------------------------------

#[test]
fn applied_and_preferences_round_trip_exactly() {
    let directory = temp_dir("roundtrip");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document()).unwrap();

    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("expected loaded document");
    };
    assert_eq!(last_settings(&document), Some(&draft_settings()));
    assert_eq!(document.preferences, preferences());

    let applied = applied_settings(draft_settings());
    let outcome = persist_verified(&mut store, &applied, preferences()).unwrap();
    assert!(outcome.is_confirmed());
    assert!(matches!(outcome.durability(), Durability::Confirmed));

    let (reloaded, reload_outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded {
        document: reloaded_document,
        ..
    } = reload_outcome
    else {
        panic!("expected reloaded document");
    };
    assert_eq!(last_settings(&reloaded_document), Some(&draft_settings()));
    assert_eq!(reloaded_document.preferences, preferences());
    assert_eq!(reloaded.snapshot(), Some(&*reloaded_document));
    // Draft state (revisions, counters) never reaches the stored bytes.
    let bytes = fs::read_to_string(settings_path(&directory)).unwrap();
    assert!(!bytes.contains("revision"), "draft state leaked: {bytes}");
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn stored_fps_is_exact_integer_rational() {
    let directory = temp_dir("exact-fps");
    let path = settings_path(&directory);

    // Write a 120/2 request: it must land normalized as 60/1, never as a
    // float, and 60000/1001 must survive untouched.
    let draft = DraftSettings {
        video: VideoRequest {
            identity: DeviceIdentity::new(
                0x1234,
                0x5678,
                UsbTopology::new("usb-wire0".to_owned(), vec![NonZeroU8::new(7).unwrap()]).unwrap(),
                None,
            )
            .unwrap(),
            mode: CaptureModeRequest {
                format_policy: InputFormatPolicy::Manual {
                    fourcc: CapturedFourCc::from_kernel(0x3231_564E),
                },
                size: FrameSize::new(1280, 720).unwrap(),
                rate: FrameRate::new(120, 2).unwrap(),
            },
            resolved_fourcc: Some(CapturedFourCc::from_kernel(0x3231_564E)),
            selected_node: None,
        },
        audio: AudioSelection::Disabled { retained: None },
        renderer: crate::domain::render::RendererSettings::default(),
    };
    let applied = applied_settings(draft);
    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Missing));
    persist_verified(&mut store, &applied, LocalPreferences::default()).unwrap();

    let bytes = fs::read_to_string(&path).unwrap();
    assert!(
        bytes.contains(r#""fps":{"numerator":60,"denominator":1}"#),
        "120/2 must normalize to 60/1: {bytes}"
    );

    // 60000/1001 keeps its exact rational form on disk.
    let applied = applied_settings(draft_settings());
    persist_verified(&mut store, &applied, preferences()).unwrap();
    let bytes = fs::read_to_string(&path).unwrap();
    assert!(
        bytes.contains(r#""fps":{"numerator":60000,"denominator":1001}"#),
        "60000/1001 must be preserved exactly: {bytes}"
    );

    let (store, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("expected loaded document");
    };
    let stored = last_settings(&document).unwrap();
    assert_eq!(stored.video.mode.rate, FrameRate::new(60000, 1001).unwrap());
    assert_eq!(stored.video.mode.rate.numerator(), 60000);
    assert_eq!(stored.video.mode.rate.denominator(), 1001);
    assert_eq!(store.snapshot(), Some(&*document));
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn preferences_only_save_preserves_published_applied() {
    let directory = temp_dir("prefs-preserve");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document()).unwrap();

    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("expected loaded document");
    };
    let applied_before = last_settings(&document).cloned();

    let new_preferences = LocalPreferences {
        gain: PlaybackGain::new(42, true).unwrap(),
        fullscreen: false,
        output: PersistentOutputChoice::Auto,
        stats_hidden: false,
        logging_enabled: false,
    };
    persist_globals(&mut store, new_preferences.clone()).unwrap();

    assert_eq!(
        last_settings(store.snapshot().unwrap()).cloned(),
        applied_before
    );
    assert_eq!(store.snapshot().unwrap().preferences, new_preferences);

    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("expected reloaded document");
    };
    assert_eq!(last_settings(&document).cloned(), applied_before);
    assert_eq!(document.preferences, new_preferences);
    fs::remove_dir_all(&directory).unwrap();
}

// ---------------------------------------------------------------------------
// Input format policy (FUR-030)
// ---------------------------------------------------------------------------

/// An automatic-policy request that already carries the concrete opening the
/// owner proved. This is the shape a prepared/verified apply persists.
fn auto_video_request() -> VideoRequest {
    let concrete = CapturedFourCc::from_kernel(0x3231_564E);
    VideoRequest {
        identity: identity(),
        mode: CaptureModeRequest {
            format_policy: InputFormatPolicy::Auto,
            size: FrameSize::new(1920, 1080).unwrap(),
            rate: FrameRate::new(60000, 1001).unwrap(),
        },
        resolved_fourcc: Some(concrete),
        selected_node: None,
    }
}

/// Inject a sibling field inside the `video` object, right after `mode`.
fn applied_with_video_suffix(suffix: &str) -> String {
    let injected = String::from(r#""denominator":1001},"#) + suffix + "}}";
    APPLIED.replace(r#""denominator":1001}}}"#, &injected)
}

#[test]
fn legacy_video_document_migrates_without_changing_the_manual_pin() {
    let directory = temp_dir("video-legacy-manual");
    let path = settings_path(&directory);
    let original = canonical_document();
    fs::write(&path, &original).unwrap();

    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("expected loaded document");
    };
    let applied = last_settings(&document).unwrap();
    let concrete = CapturedFourCc::from_kernel(0x3231_564E);
    assert_eq!(
        applied.video.mode.format_policy,
        InputFormatPolicy::Manual { fourcc: concrete }
    );
    assert_eq!(applied.video.resolved_fourcc, Some(concrete));
    assert_eq!(applied.video.selected_node, None);

    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    persist_verified(
        &mut store,
        &applied_settings(draft_settings()),
        preferences(),
    )
    .unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: migrated, ..
    } = outcome
    else {
        panic!("migrated profile must load")
    };
    assert_eq!(last_settings(&migrated), last_settings(&document));
    assert_eq!(
        migrated.profiles.entries()[0].binding(),
        ProfileBinding::Serial
    );
    assert_eq!(migrated.last_source.as_ref(), Some(&identity()));
    assert!(
        !fs::read_to_string(&path).unwrap().contains("format_policy"),
        "manual policy retains its existing representation"
    );
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn auto_video_policy_round_trips_with_marker_and_historical_concrete() {
    let directory = temp_dir("video-auto-roundtrip");
    let path = settings_path(&directory);
    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Missing));

    let mut draft = draft_settings();
    draft.video = auto_video_request();
    let applied = applied_settings(draft);
    let expected = applied.settings().video.clone();
    assert_eq!(expected.mode.format_policy, InputFormatPolicy::Auto);

    persist_verified(&mut store, &applied, preferences()).unwrap();
    assert_eq!(
        last_settings(store.snapshot().unwrap()).unwrap().video,
        expected
    );

    // Parsed shape: the marker is a `format_policy` sibling of `mode`, and the
    // concrete opening is still stored in `mode.captured_fourcc`. The
    // session-only node locator never reaches disk.
    let bytes = fs::read_to_string(&path).unwrap();
    let document: serde_json::Value = serde_json::from_str(&bytes).unwrap();
    let video = &document["profiles"][0]["settings"]["video"];
    assert_eq!(video["format_policy"], "auto");
    assert_eq!(
        video["mode"]["captured_fourcc"],
        serde_json::json!(842094158)
    );
    assert!(
        !bytes.contains("selected_node"),
        "node locator leaked: {bytes}"
    );

    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("auto document must reload");
    };
    let stored = last_settings(&document).unwrap();
    assert_eq!(stored.video.mode.format_policy, InputFormatPolicy::Auto);
    assert_eq!(
        stored.video.resolved_fourcc,
        Some(CapturedFourCc::from_kernel(0x3231_564E))
    );
    assert_eq!(stored.video, expected);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn strict_format_policy_schema_violations_are_refused_individually() {
    let directory = temp_dir("video-policy-refusals");
    let cases: &[(&str, &str)] = &[
        ("null marker", r#""format_policy":null"#),
        ("manual literal", r#""format_policy":"manual""#),
        ("wrong case marker", r#""format_policy":"Auto""#),
        ("numeric marker", r#""format_policy":1"#),
        ("boolean marker", r#""format_policy":true"#),
        ("array marker", r#""format_policy":["auto"]"#),
        ("object marker", r#""format_policy":{}"#),
        ("unknown video field", r#""future":true"#),
        (
            "duplicate marker",
            r#""format_policy":"auto","format_policy":"auto""#,
        ),
    ];
    for (label, suffix) in cases {
        let json = document_with_applied(&applied_with_video_suffix(suffix));
        refused_schema(&directory, label, &json);
    }
    fs::remove_dir_all(&directory).unwrap();
}

/// The `"auto"` marker document for `kernel`: the same shape a verified apply
/// persists (auto marker plus the concrete opening it proved). Generated from
/// the pinned literal `APPLIED` fixture through `serde_json::Value` so the
/// document is structurally valid by construction while the concrete opening is
/// set as a real integer; the marker is a sibling of `mode`, matching the
/// writer's `format_policy` placement.
fn auto_marker_document(kernel: u32) -> String {
    let mut applied: serde_json::Value = serde_json::from_str(APPLIED).unwrap();
    applied["video"]["format_policy"] = serde_json::Value::String("auto".to_owned());
    applied["video"]["mode"]["captured_fourcc"] = serde_json::Value::from(kernel);
    document_with_applied(&applied.to_string())
}

#[test]
fn auto_policy_with_non_concrete_fourcc_is_refused_at_decode_and_blocks_saves() {
    // Presence guard: the automatic marker paired with an admissible concrete
    // opening decodes, so the refusals below exercise the value boundary rather
    // than a structurally broken fixture.
    let directory = temp_dir("video-auto-decode-refusals");
    let path = settings_path(&directory);
    let valid = auto_marker_document(842094158);
    fs::write(&path, &valid).unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    assert!(
        matches!(outcome, LoadOutcome::Loaded { .. }),
        "admissible auto document must load: {outcome:?}"
    );

    // The automatic policy only ever resolves to YUYV/NV12/MJPG. BGR3 is a
    // manual-only opening, so an auto marker claiming it describes no verified
    // opening at all. This is the exact state a `SettingsStore` snapshot keeps
    // after startup clears an auto mode: the on-disk baseline must be refused
    // rather than silently reopened on another node or leaving a poison value
    // whose preference-only save would fail to re-encode.
    let unknown = 0x1234_5678;
    for (label, kernel) in [
        ("auto with manual-only BGR3", 861030210u32),
        ("auto with unknown FourCC", unknown),
    ] {
        let json = auto_marker_document(kernel);
        fs::write(&path, &json).unwrap();
        let original = fs::read(&path).unwrap();
        let (mut store, outcome) = SettingsStore::load(path.clone());
        assert!(
            matches!(
                outcome,
                LoadOutcome::Refused(SettingsLoadError::InvalidValue { .. })
            ),
            "{label}: expected InvalidValue refusal, got {outcome:?}"
        );
        assert!(store.is_refused(), "{label}");
        assert!(store.snapshot().is_none(), "{label}");

        // Neither a preference-only save nor a full applied save may rewrite the
        // refused baseline; the exact original bytes survive.
        assert!(
            persist_globals(&mut store, preferences()).is_err(),
            "{label}: preference save must be blocked"
        );
        assert!(
            persist_verified(
                &mut store,
                &applied_settings(draft_settings()),
                preferences()
            )
            .is_err(),
            "{label}: applied save must be blocked"
        );
        assert_eq!(fs::read(&path).unwrap(), original, "{label}");
        assert_no_temp_files(&directory);
    }
    fs::remove_dir_all(&directory).unwrap();
}

/// Encode a constructed document directly, bypassing the model, and return the
/// typed refusal. This is the serialization boundary the invariant protects.
fn encode_video_error(video: VideoRequest) -> crate::settings::schema::EncodeError {
    let settings = DraftSettings {
        video,
        ..draft_settings()
    };
    let last_source = Some(settings.video.identity.clone());
    let document = StoredDocument {
        profiles: SourceProfiles::new(vec![
            SourceProfile::new(ProfileBinding::Serial, settings).unwrap(),
        ])
        .unwrap(),
        last_source,
        preferences: preferences(),
    };
    crate::settings::schema::encode(&document).unwrap_err()
}

#[test]
fn unresolved_or_inconsistent_video_never_serializes() {
    use crate::settings::schema::EncodeError;

    // An unprepared policy keeps no concrete format: it must refuse atomically
    // rather than invent a FourCC or panic.
    let unresolved = VideoRequest {
        resolved_fourcc: None,
        selected_node: Some(PathBuf::from("/dev/video0")),
        ..auto_video_request()
    };
    assert!(
        matches!(
            encode_video_error(unresolved),
            EncodeError::InvalidValue(SettingsValidationError::UnresolvedVideo)
        ),
        "unresolved video must refuse"
    );

    // A manual pin that disagrees with the proven concrete is not a verified
    // opening either.
    let mut mismatched = auto_video_request();
    mismatched.mode.format_policy = InputFormatPolicy::Manual {
        fourcc: CapturedFourCc::from_bytes(*b"YUYV"),
    };
    assert!(
        matches!(
            encode_video_error(mismatched),
            EncodeError::InvalidValue(SettingsValidationError::VideoFormatPolicyMismatch)
        ),
        "mismatched manual pin must refuse"
    );

    // The automatic policy only ever resolves to YUYV/NV12/MJPG: a manual-only
    // FourCC or an unrecognized kernel value must refuse, matching the domain
    // resolution invariant instead of persisting a fake automatic proof.
    let mut bgr3_auto = auto_video_request();
    bgr3_auto.resolved_fourcc = Some(CapturedFourCc::from_bytes(*b"BGR3"));
    assert!(
        matches!(
            encode_video_error(bgr3_auto),
            EncodeError::InvalidValue(SettingsValidationError::VideoFormatPolicyMismatch)
        ),
        "auto with a manual-only FourCC must refuse"
    );

    let mut unknown_auto = auto_video_request();
    unknown_auto.resolved_fourcc = Some(CapturedFourCc::from_kernel(0x1234_5678));
    assert!(
        matches!(
            encode_video_error(unknown_auto),
            EncodeError::InvalidValue(SettingsValidationError::VideoFormatPolicyMismatch)
        ),
        "auto with an unrecognized FourCC must refuse"
    );
}

#[test]
fn manual_bgr3_video_encoding_is_preserved() {
    let directory = temp_dir("video-manual-bgr3");
    let path = settings_path(&directory);
    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Missing));

    let bgr3 = CapturedFourCc::from_bytes(*b"BGR3");
    let draft = DraftSettings {
        video: VideoRequest {
            mode: CaptureModeRequest {
                format_policy: InputFormatPolicy::Manual { fourcc: bgr3 },
                size: FrameSize::new(1920, 1080).unwrap(),
                rate: FrameRate::new(60000, 1001).unwrap(),
            },
            resolved_fourcc: Some(bgr3),
            ..draft_settings().video
        },
        ..draft_settings()
    };
    let applied = applied_settings(draft);
    persist_verified(&mut store, &applied, preferences()).unwrap();

    // A manual pin keeps writing its concrete FourCC and never adds a marker.
    let bytes = fs::read_to_string(&path).unwrap();
    assert!(
        !bytes.contains("format_policy"),
        "manual pin must not write a marker: {bytes}"
    );
    let document: serde_json::Value = serde_json::from_str(&bytes).unwrap();
    assert_eq!(
        document["profiles"][0]["settings"]["video"]["mode"]["captured_fourcc"],
        serde_json::json!(bgr3.kernel_value())
    );

    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("manual BGR3 document must reload");
    };
    let stored = last_settings(&document).unwrap();
    assert_eq!(
        stored.video.mode.format_policy,
        InputFormatPolicy::Manual { fourcc: bgr3 }
    );
    assert_eq!(stored.video.resolved_fourcc, Some(bgr3));
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn preferences_only_save_on_missing_file_writes_null_applied() {
    let directory = temp_dir("prefs-missing");
    let path = settings_path(&directory);
    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Missing));

    persist_globals(&mut store, preferences()).unwrap();
    let bytes = fs::read_to_string(&path).unwrap();
    assert!(
        !bytes.contains(r#""applied""#),
        "legacy payload leaked: {bytes}"
    );
    assert!(store.snapshot().unwrap().profiles.entries().is_empty());
    assert!(store.snapshot().unwrap().last_source.is_none());
    assert_eq!(store.snapshot().unwrap().preferences, preferences());
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn stats_visibility_defaults_and_persists_without_changing_applied() {
    let directory = temp_dir("stats-visibility");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document()).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: original, ..
    } = outcome
    else {
        panic!("legacy document must load");
    };
    assert!(!original.preferences.stats_hidden);
    let mut hidden = original.preferences.clone();
    hidden.stats_hidden = true;
    persist_globals(&mut store, hidden.clone()).unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: reloaded, ..
    } = outcome
    else {
        panic!("hidden preference must reload");
    };
    assert_eq!(reloaded.profiles, original.profiles);
    assert_eq!(reloaded.last_source, original.last_source);
    assert_eq!(reloaded.preferences, hidden);
    persist_verified(
        &mut store,
        &applied_settings(draft_settings()),
        hidden.clone(),
    )
    .unwrap();
    assert!(store.snapshot().unwrap().preferences.stats_hidden);
    store.reset(hidden.clone()).unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: reset, ..
    } = outcome
    else {
        panic!("reset preference must reload");
    };
    assert!(reset.profiles.entries().is_empty());
    assert!(reset.last_source.is_none());
    assert_eq!(reset.preferences, hidden);
    hidden.stats_hidden = false;
    persist_globals(&mut store, hidden).unwrap();
    let document: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(document["preferences"].get("stats_hidden").is_none());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn invalid_stats_visibility_preserves_original_and_refuses_automatic_saves() {
    let directory = temp_dir("stats-visibility-refusals");
    for value in ["null", "\"true\"", "1", "{}", "true,\"stats_hidden\":false"] {
        let document = format!(
            r#"{{"schema_version":1,"applied":null,"preferences":{{"volume_percent":73,"muted":false,"fullscreen":true,"stats_hidden":{value}}}}}"#
        );
        let (mut store, outcome) = refused_schema(&directory, value, &document);
        assert!(matches!(outcome, LoadOutcome::Refused(_)));
        assert!(persist_globals(&mut store, preferences()).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), document.as_bytes());
    }
    fs::remove_dir_all(directory).unwrap();
}

// ---------------------------------------------------------------------------
// Diagnostic logging opt-in preference
// ---------------------------------------------------------------------------

#[test]
fn logging_preference_absent_from_every_supported_version_starts_off() {
    for version in [1, 2, 3] {
        let directory = temp_dir("logging-absent");
        let path = settings_path(&directory);
        let document = if version == 1 {
            canonical_document()
        } else {
            v2_document("[]", "null").replacen(
                r#""schema_version":2"#,
                &format!(r#""schema_version":{version}"#),
                1,
            )
        };
        fs::write(&path, document).unwrap();
        let (_, outcome) = SettingsStore::load(path);
        let LoadOutcome::Loaded { document, .. } = outcome else {
            panic!("v{version} document must load")
        };
        assert!(!document.preferences.logging_enabled);
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn logging_opt_in_is_written_only_as_explicit_true_and_false_is_omitted() {
    let directory = temp_dir("logging-roundtrip");
    let path = settings_path(&directory);
    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Missing));

    let mut opted_in = preferences();
    opted_in.logging_enabled = true;
    persist_globals(&mut store, opted_in.clone()).unwrap();
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains(r#""logging_enabled":true"#)
    );
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: reloaded, ..
    } = outcome
    else {
        panic!("opt-in preference must reload");
    };
    assert_eq!(reloaded.preferences, opted_in);

    opted_in.logging_enabled = false;
    persist_globals(&mut store, opted_in.clone()).unwrap();
    let document: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(document["preferences"].get("logging_enabled").is_none());
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document: off, .. } = outcome else {
        panic!("off preference must reload");
    };
    assert_eq!(off.preferences, opted_in);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn logging_opt_in_survives_reset_and_leaves_profiles_and_other_globals_unchanged() {
    let directory = temp_dir("logging-reset");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document()).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: original, ..
    } = outcome
    else {
        panic!("legacy document must load");
    };
    let mut opted_in = original.preferences.clone();
    opted_in.logging_enabled = true;
    persist_verified(
        &mut store,
        &applied_settings(draft_settings()),
        opted_in.clone(),
    )
    .unwrap();
    assert!(store.snapshot().unwrap().preferences.logging_enabled);
    store.reset(opted_in.clone()).unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: reset, ..
    } = outcome
    else {
        panic!("reset preference must reload");
    };
    assert!(reset.profiles.entries().is_empty());
    assert!(reset.last_source.is_none());
    assert_eq!(reset.preferences, opted_in);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn invalid_logging_preference_is_refused_without_best_effort_extraction() {
    let directory = temp_dir("logging-refusals");
    for value in [
        "null",
        "\"true\"",
        "1",
        "{}",
        "true,\"logging_enabled\":false",
    ] {
        let document = format!(
            r#"{{"schema_version":1,"applied":null,"preferences":{{"volume_percent":73,"muted":false,"fullscreen":true,"logging_enabled":{value}}}}}"#
        );
        let (mut store, outcome) = refused_schema(&directory, value, &document);
        assert!(matches!(outcome, LoadOutcome::Refused(_)));
        assert!(persist_globals(&mut store, preferences()).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), document.as_bytes());
    }
    // A refused document carrying the true opt-in beside an unrelated invalid
    // field stays refused: the flag is never extracted best-effort.
    let poisoned = r#"{"schema_version":1,"applied":null,"preferences":{"volume_percent":73,"muted":false,"fullscreen":true,"logging_enabled":true,"output":null}}"#;
    let (store, outcome) = refused_schema(&directory, "refused-true-flag", poisoned);
    assert!(matches!(outcome, LoadOutcome::Refused(_)));
    assert!(store.snapshot().is_none());
    // An explicit false is a valid document and still means off.
    let explicit_false = r#"{"schema_version":1,"applied":null,"preferences":{"volume_percent":73,"muted":false,"fullscreen":true,"logging_enabled":false}}"#;
    let path = settings_path(&directory);
    fs::write(&path, explicit_false).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("explicit false must load");
    };
    assert!(!document.preferences.logging_enabled);
    persist_globals(&mut store, document.preferences).unwrap();
    let wire: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(wire["preferences"].get("logging_enabled").is_none());
    fs::remove_dir_all(directory).unwrap();
}

// ---------------------------------------------------------------------------
// Output choice preference
// ---------------------------------------------------------------------------

fn manual_sink_identity() -> SinkIdentity {
    SinkIdentity::new(
        "easyeffects_sink".to_owned(),
        vec![("device.api".to_owned(), "pipewire".to_owned())],
    )
    .unwrap()
}

fn manual_output_preferences() -> LocalPreferences {
    LocalPreferences {
        gain: PlaybackGain::new(73, false).unwrap(),
        fullscreen: true,
        output: PersistentOutputChoice::Manual(manual_sink_identity()),
        stats_hidden: false,
        logging_enabled: false,
    }
}

fn refused_output_document(directory: &Path, label: &str, preferences_json: &str) {
    let document =
        format!(r#"{{"schema_version":1,"applied":null,"preferences":{preferences_json}}}"#);
    let (store, outcome) = refused_schema(directory, label, &document);
    assert!(matches!(outcome, LoadOutcome::Refused(_)), "{label}");
    assert!(store.is_refused(), "{label}");
}

#[test]
fn manual_output_choice_round_trips_exactly() {
    let directory = temp_dir("manual-output-roundtrip");
    let path = settings_path(&directory);
    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Missing));

    let manual = manual_output_preferences();
    persist_globals(&mut store, manual.clone()).unwrap();

    // Parsed shape, not serializer byte layout: the manual choice is a
    // kind-tagged sink object next to the legacy preference fields.
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let preferences = &document["preferences"];
    assert_eq!(preferences["output"]["kind"], "manual");
    assert_eq!(preferences["output"]["sink"]["name"], "easyeffects_sink");
    assert_eq!(
        preferences["output"]["sink"]["stable_properties"],
        serde_json::json!([{ "key": "device.api", "value": "pipewire" }])
    );

    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("manual output document must reload");
    };
    assert_eq!(document.preferences, manual);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn auto_output_choice_omits_the_output_key_and_defaults_from_missing() {
    let directory = temp_dir("auto-output-omits-key");
    let path = settings_path(&directory);
    let (mut store, _) = SettingsStore::load(path.clone());

    persist_globals(&mut store, preferences()).unwrap();
    // Parsed shape: Auto omits the `output` key entirely.
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        document["preferences"].get("output").is_none(),
        "Auto must not persist an output key: {document}"
    );

    // The literal pre-output v1 document (no `output` key) decodes to Auto.
    let legacy = format!(r#"{{"schema_version":1,"applied":null,"preferences":{PREFS}}}"#);
    fs::write(&path, legacy.as_bytes()).unwrap();
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("legacy v1 document must reload");
    };
    assert_eq!(document.preferences.output, PersistentOutputChoice::Auto);
    assert_eq!(document.preferences, preferences());
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn strict_output_choice_schema_violations_are_refused_individually() {
    let directory = temp_dir("output-schema-refusals");
    let volume = r#""volume_percent":73,"muted":false,"fullscreen":true"#;
    let sink = r#""name":"easyeffects_sink","stable_properties":[{"key":"device.api","value":"pipewire"}]"#;

    let cases: &[(&str, String)] = &[
        // Unknown/future field anywhere in the output subtree.
        (
            "unknown future field in output",
            format!(r#"{{{volume},"output":{{"kind":"auto","future":true}}}}"#),
        ),
        (
            "unknown future field in sink",
            format!(r#"{{{volume},"output":{{"kind":"manual","sink":{{"future":1,{sink}}}}}}}"#),
        ),
        // Incomplete manual choice.
        (
            "manual without sink",
            format!(r#"{{{volume},"output":{{"kind":"manual"}}}}"#),
        ),
        (
            "manual with null sink",
            format!(r#"{{{volume},"output":{{"kind":"manual","sink":null}}}}"#),
        ),
        (
            "output without kind",
            format!(r#"{{{volume},"output":{{}}}}"#),
        ),
        // Legal sibling fields on the wrong variant.
        (
            "auto with sink",
            format!(r#"{{{volume},"output":{{"kind":"auto","sink":{{{sink}}}}}}}"#),
        ),
        // Unknown kind: a future variant, not Auto.
        (
            "unknown future kind",
            format!(r#"{{{volume},"output":{{"kind":"spatial","sink":{{{sink}}}}}}}"#),
        ),
        // Duplicate and invalid shapes.
        (
            "duplicate output key",
            format!(r#"{{{volume},"output":{{"kind":"auto"}},"output":{{"kind":"auto"}}}}"#),
        ),
        ("null output", format!(r#"{{{volume},"output":null}}"#)),
        (
            "sequence output",
            format!(r#"{{{volume},"output":["auto"]}}"#),
        ),
        // Identity-level invalid values: refused after the schema pass.
        (
            "sink property outside whitelist",
            format!(
                r#"{{{volume},"output":{{"kind":"manual","sink":{{"name":"easyeffects_sink","stable_properties":[{{"key":"node.cpu_time","value":"9"}}]}}}}}}"#
            ),
        ),
        (
            "duplicate sink property key",
            format!(
                r#"{{{volume},"output":{{"kind":"manual","sink":{{"name":"easyeffects_sink","stable_properties":[{{"key":"device.api","value":"pipewire"}},{{"key":"device.api","value":"alsa"}}]}}}}}}"#
            ),
        ),
        (
            "empty sink name",
            format!(
                r#"{{{volume},"output":{{"kind":"manual","sink":{{"name":"","stable_properties":[]}}}}}}"#
            ),
        ),
    ];

    for (label, preferences_json) in cases {
        refused_output_document(&directory, label, preferences_json);
    }

    // A refusal blocks the automatic preference save and preserves the bytes.
    let path = settings_path(&directory);
    let original = fs::read(&path).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Refused(_)));
    assert!(matches!(
        persist_globals(&mut store, manual_output_preferences()),
        Err(SettingsWriteError::RefusedOriginal { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_no_temp_files(&directory);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn manual_output_preference_save_preserves_applied_and_unrelated_preferences() {
    let directory = temp_dir("manual-output-preserves-applied");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document().as_bytes()).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: previous, ..
    } = outcome
    else {
        panic!("expected valid original");
    };
    let applied_before = last_settings(&previous).cloned();

    // Only the output choice changes; gain/fullscreen and the published
    // applied document are preserved through a preferences-only save.
    let mut switched = previous.preferences.clone();
    switched.output = PersistentOutputChoice::Manual(manual_sink_identity());
    persist_globals(&mut store, switched.clone()).unwrap();
    assert_eq!(
        last_settings(store.snapshot().unwrap()).cloned(),
        applied_before
    );
    assert_eq!(store.snapshot().unwrap().preferences, switched);

    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("manual output document must reload");
    };
    assert_eq!(last_settings(&document).cloned(), applied_before);
    assert_eq!(
        document.preferences.gain, previous.preferences.gain,
        "unrelated preference value preserved"
    );
    assert_eq!(
        document.preferences.fullscreen,
        previous.preferences.fullscreen
    );
    assert_eq!(document.preferences.output, switched.output);
    fs::remove_dir_all(&directory).unwrap();
}

// ---------------------------------------------------------------------------
// Refusal blocks automatic writes
// ---------------------------------------------------------------------------

#[test]
fn refusal_blocks_all_collection_saves_byte_for_byte() {
    let directory = temp_dir("refusal-blocks");
    let path = settings_path(&directory);
    fs::write(&path, "{corrupt").unwrap();
    let original = fs::read(&path).unwrap();

    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Refused(_)));
    assert!(store.is_refused());
    assert_eq!(store.snapshot(), None);

    let applied = applied_settings(draft_settings());
    assert!(matches!(
        persist_verified(&mut store, &applied, preferences()),
        Err(SettingsWriteError::RefusedOriginal { .. })
    ));
    assert!(matches!(
        persist_globals(&mut store, preferences()),
        Err(SettingsWriteError::RefusedOriginal { .. })
    ));

    assert_eq!(fs::read(&path).unwrap(), original);
    assert_no_temp_files(&directory);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn invalid_in_memory_preferences_preserve_file_and_published_snapshot() {
    let directory = temp_dir("invalid-in-memory-preferences");
    let path = settings_path(&directory);
    let original = canonical_document();
    fs::write(&path, original.as_bytes()).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: previous, ..
    } = outcome
    else {
        panic!("expected valid original");
    };
    let applied = applied_settings(draft_settings());
    let invalid = LocalPreferences {
        gain: PlaybackGain {
            volume_percent: 101,
            muted: true,
        },
        fullscreen: false,
        output: PersistentOutputChoice::Auto,
        stats_hidden: false,
        logging_enabled: false,
    };
    for operation in ["applied", "preferences", "reset"] {
        let result = match operation {
            "applied" => persist_verified(&mut store, &applied, invalid.clone()),
            "preferences" => persist_globals(&mut store, invalid.clone()),
            "reset" => store.reset(invalid.clone()),
            _ => unreachable!(),
        };
        assert!(
            matches!(result, Err(SettingsWriteError::InvalidValue { .. })),
            "{operation}"
        );
        assert_eq!(store.snapshot(), Some(&*previous), "{operation}");
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes(), "{operation}");
        assert_no_temp_files(&directory);
    }
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn failed_reset_preserves_refusal_original_and_blocks_automatic_saves() {
    let directory = temp_dir("failed-reset");
    let path = settings_path(&directory);
    let original = br#"{"schema_version":4,"future":"preserve this original"}"#;
    fs::write(&path, original).unwrap();
    let mut store = SettingsStore::with_injected_fault(path.clone(), WriteFaultPoint::PartialWrite);
    assert!(store.is_refused());

    let invalid = LocalPreferences {
        gain: PlaybackGain {
            volume_percent: 101,
            muted: true,
        },
        fullscreen: false,
        output: PersistentOutputChoice::Auto,
        stats_hidden: false,
        logging_enabled: false,
    };
    assert!(matches!(
        store.reset(invalid),
        Err(SettingsWriteError::InvalidValue { .. })
    ));
    assert!(store.is_refused());
    assert!(store.snapshot().is_none());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_no_temp_files(&directory);

    assert!(matches!(
        store.reset(preferences()),
        Err(SettingsWriteError::WriteTemporary { .. })
    ));
    assert!(store.is_refused());
    assert!(store.snapshot().is_none());
    assert_eq!(fs::read(&path).unwrap(), original);
    let (_, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(
        outcome,
        LoadOutcome::Refused(SettingsLoadError::UnsupportedVersion { version: 4, .. })
    ));
    let applied = applied_settings(draft_settings());
    assert!(matches!(
        persist_verified(&mut store, &applied, preferences()),
        Err(SettingsWriteError::RefusedOriginal { .. })
    ));
    assert!(matches!(
        persist_globals(&mut store, preferences()),
        Err(SettingsWriteError::RefusedOriginal { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_no_temp_files(&directory);
    fs::remove_dir_all(&directory).unwrap();
}

// ---------------------------------------------------------------------------
// Atomic writer faults
// ---------------------------------------------------------------------------

#[test]
fn precommit_faults_leave_original_file_intact_and_loadable() {
    let points = [
        WriteFaultPoint::BeforeWrite,
        WriteFaultPoint::PartialWrite,
        WriteFaultPoint::BeforeTemporarySync,
        WriteFaultPoint::TemporarySyncIo,
        WriteFaultPoint::BeforeRename,
        WriteFaultPoint::RenameIo,
    ];
    for point in points {
        let directory = temp_dir("fault");
        let path = settings_path(&directory);
        fs::write(&path, canonical_document()).unwrap();
        let original = fs::read(&path).unwrap();

        let mut store = SettingsStore::with_injected_fault(path.clone(), point);
        let previous = store.snapshot().unwrap().clone();
        let mut attempted = draft_settings();
        attempted.video = video_request(0x3231_564E, 1280, 720, 60, 1);
        attempted.audio = AudioSelection::Disabled { retained: None };
        let applied = applied_settings(attempted);
        let attempted_preferences = LocalPreferences {
            gain: PlaybackGain::new(42, true).unwrap(),
            fullscreen: false,
            output: PersistentOutputChoice::Auto,
            stats_hidden: false,
            logging_enabled: false,
        };
        let result = persist_verified(&mut store, &applied, attempted_preferences.clone());
        let expected_error = match point {
            WriteFaultPoint::BeforeWrite | WriteFaultPoint::PartialWrite => {
                matches!(result, Err(SettingsWriteError::WriteTemporary { .. }))
            }
            WriteFaultPoint::BeforeTemporarySync | WriteFaultPoint::TemporarySyncIo => {
                matches!(result, Err(SettingsWriteError::SyncTemporary { .. }))
            }
            WriteFaultPoint::BeforeRename | WriteFaultPoint::RenameIo => {
                matches!(result, Err(SettingsWriteError::Rename { .. }))
            }
            _ => unreachable!(),
        };
        assert!(expected_error, "{point:?}: unexpected result {result:?}");
        assert_eq!(store.snapshot(), Some(&previous), "{point:?}");
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "{point:?}: original bytes must survive"
        );
        assert_no_temp_files(&directory);
        let (_, reloaded) = SettingsStore::load(path.clone());
        let LoadOutcome::Loaded {
            document: reloaded, ..
        } = reloaded
        else {
            panic!("{point:?}: original must stay loadable immediately after failure");
        };
        assert_eq!(*reloaded, previous, "{point:?}");

        // The fault fires once: the next write is a normal committed write.
        let outcome = persist_globals(&mut store, attempted_preferences.clone()).unwrap();
        assert!(outcome.is_confirmed(), "{point:?}");
        let (_, outcome) = SettingsStore::load(path);
        let LoadOutcome::Loaded { document, .. } = outcome else {
            panic!("{point:?}: next preference write must load");
        };
        assert_eq!(document.profiles, previous.profiles, "{point:?}");
        assert_eq!(document.last_source, previous.last_source, "{point:?}");
        assert_eq!(document.preferences, attempted_preferences, "{point:?}");
        fs::remove_dir_all(&directory).unwrap();
    }
}

#[test]
fn partial_write_never_leaves_half_content_in_place() {
    let directory = temp_dir("partial");
    let path = settings_path(&directory);
    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Missing));
    persist_globals(&mut store, preferences()).unwrap();
    let complete = fs::read(&path).unwrap();

    let mut store = SettingsStore::with_injected_fault(path.clone(), WriteFaultPoint::PartialWrite);
    let previous = store.snapshot().unwrap().clone();
    let applied = applied_settings(draft_settings());
    let attempted_preferences = LocalPreferences {
        gain: PlaybackGain::new(42, true).unwrap(),
        fullscreen: false,
        output: PersistentOutputChoice::Auto,
        stats_hidden: false,
        logging_enabled: false,
    };
    assert!(matches!(
        persist_verified(&mut store, &applied, attempted_preferences),
        Err(SettingsWriteError::WriteTemporary { .. })
    ));
    // The target and published snapshot retain the previous complete document.
    assert_eq!(fs::read(&path).unwrap(), complete);
    assert_eq!(store.snapshot(), Some(&previous));
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("partial write must leave the original immediately loadable");
    };
    assert_eq!(*document, previous);
    assert_no_temp_files(&directory);
    fs::remove_dir_all(&directory).unwrap();
}

// ---------------------------------------------------------------------------
// Explicit reset
// ---------------------------------------------------------------------------

#[test]
fn explicit_reset_clears_refusal_and_writes_null_applied() {
    let directory = temp_dir("reset");
    let path = settings_path(&directory);
    fs::write(&path, "garbage").unwrap();

    let (mut store, _) = SettingsStore::load(path.clone());
    assert!(store.is_refused());

    let outcome = store.reset(preferences()).unwrap();
    assert!(outcome.is_confirmed());
    assert!(!store.is_refused());
    assert!(store.snapshot().unwrap().profiles.entries().is_empty());
    assert!(store.snapshot().unwrap().last_source.is_none());
    assert_eq!(store.snapshot().unwrap().preferences, preferences());

    let (mut reloaded, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("reset must produce a loadable file");
    };
    assert!(document.profiles.entries().is_empty());
    assert!(document.last_source.is_none());
    assert_eq!(document.preferences, preferences());

    // After reset, a preference save keeps applied explicitly null.
    let later = LocalPreferences {
        gain: PlaybackGain::new(15, true).unwrap(),
        fullscreen: false,
        output: PersistentOutputChoice::Auto,
        stats_hidden: false,
        logging_enabled: false,
    };
    persist_globals(&mut reloaded, later.clone()).unwrap();
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("expected loaded document after later save");
    };
    assert!(document.profiles.entries().is_empty());
    assert!(document.last_source.is_none());
    assert_eq!(document.preferences, later);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn reset_refuses_nothing_about_the_previous_content_and_is_explicit_only() {
    let directory = temp_dir("reset-explicit");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document()).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { .. } = outcome else {
        panic!("expected loaded document");
    };

    // A full collection save changing only globals must not clear profiles.
    persist_globals(&mut store, LocalPreferences::default()).unwrap();
    assert_eq!(store.snapshot().unwrap().profiles.entries().len(), 1);

    // Only reset replaces the whole document.
    store.reset(LocalPreferences::default()).unwrap();
    assert!(store.snapshot().unwrap().profiles.entries().is_empty());
    assert!(store.snapshot().unwrap().last_source.is_none());
    fs::remove_dir_all(&directory).unwrap();
}

// ---------------------------------------------------------------------------
// Post-commit durability uncertainty
// ---------------------------------------------------------------------------

#[test]
fn parent_sync_uncertainty_is_committed_but_unconfirmed() {
    for point in [WriteFaultPoint::ParentOpenIo, WriteFaultPoint::ParentSyncIo] {
        let directory = temp_dir("unconfirmed");
        let path = settings_path(&directory);
        let mut store = SettingsStore::with_injected_fault(path.clone(), point);

        let applied = applied_settings(draft_settings());
        let outcome = persist_verified(&mut store, &applied, preferences()).unwrap();
        let WriteOutcome::Committed { durability } = &outcome;
        assert!(!outcome.is_confirmed(), "{point:?}");
        assert!(
            matches!(durability, Durability::Unconfirmed(_)),
            "{point:?}"
        );

        // The file is complete and loadable, and the store published it.
        let (reloaded, outcome) = SettingsStore::load(path);
        let LoadOutcome::Loaded { document, .. } = outcome else {
            panic!("{point:?}: committed file must load");
        };
        assert_eq!(last_settings(&document), Some(&draft_settings()));
        assert_eq!(store.snapshot(), Some(&*document));
        assert_eq!(document.preferences, preferences());
        assert_eq!(reloaded.snapshot(), Some(&*document));
        assert_eq!(reloaded.snapshot().unwrap().profiles.entries().len(), 1);
        fs::remove_dir_all(&directory).unwrap();
    }
}

// ---------------------------------------------------------------------------
// File hygiene
// ---------------------------------------------------------------------------

#[test]
fn written_settings_file_is_owner_only() {
    let directory = temp_dir("perms");
    let path = settings_path(&directory);
    let (mut store, _) = SettingsStore::load(path.clone());
    persist_globals(&mut store, preferences()).unwrap();
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "settings must be 0600, got {mode:o}");
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn temporary_files_are_never_loaded_as_settings() {
    let directory = temp_dir("tmp-ignore");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document()).unwrap();
    let stray = directory.join("settings.json.furami-tmp-424242-7");
    fs::write(&stray, "garbage").unwrap();

    let (store, outcome) = SettingsStore::load(path);
    assert!(
        matches!(outcome, LoadOutcome::Loaded { .. }),
        "stray temp file must not affect loading: {outcome:?}"
    );
    assert!(!store.is_refused());
    fs::remove_dir_all(&directory).unwrap();
}

// ---------------------------------------------------------------------------
// Retired settings migration: legacy reads are lazy, current writes are strict
// ---------------------------------------------------------------------------

/// Historical wire payload, intentionally not represented by a domain model.
const RETIRED_FILTERS: &str = r#"{"entries":[{"label":"Levels","enabled":true,"filter":{"kind":"format","parameters":{"matrix":"bt709","levels":"limited","gamma":"bt1886"}}}]}"#;

fn applied_with_filters(filters_json: &str) -> String {
    let inner = APPLIED.strip_suffix('}').unwrap();
    format!(r#"{inner},"filters":{filters_json}}}"#)
}

#[test]
fn legacy_retired_values_are_ignored_lazily_and_v3_preserves_every_remaining_field() {
    let directory = temp_dir("retired-settings-migration");
    let path = settings_path(&directory);
    let (a, b) = two_profile_settings();
    let globals = LocalPreferences {
        gain: PlaybackGain::new(0, true).unwrap(),
        fullscreen: true,
        output: PersistentOutputChoice::Manual(manual_sink_identity()),
        stats_hidden: true,
        logging_enabled: true,
    };
    for version in [1, 2] {
        let entries = if version == 1 {
            vec![SourceProfile::new(ProfileBinding::Serial, a.clone()).unwrap()]
        } else {
            vec![
                SourceProfile::new(ProfileBinding::Serial, a.clone()).unwrap(),
                SourceProfile::new(ProfileBinding::Topology, b.clone()).unwrap(),
            ]
        };
        let expected = StoredDocument {
            profiles: SourceProfiles::new(entries).unwrap(),
            last_source: Some(if version == 1 {
                a.video.identity.clone()
            } else {
                b.video.identity.clone()
            }),
            preferences: globals.clone(),
        };
        let current = crate::settings::schema::encode(&expected).unwrap();
        for retired in [
            None,
            Some(RETIRED_FILTERS),
            Some("null"),
            Some("false"),
            Some("42"),
            Some(r#""arbitrary legacy text""#),
            Some("[]"),
            Some(r#"{"entries":"invalid","future":{"duplicate":1,"duplicate":2}}"#),
        ] {
            let mut wire: serde_json::Value = serde_json::from_slice(&current).unwrap();
            wire["schema_version"] = version.into();
            if version == 1 {
                let applied = wire["profiles"][0]["settings"].take();
                let object = wire.as_object_mut().unwrap();
                object.remove("profiles");
                object.remove("last_source");
                object.insert("applied".into(), applied);
            }
            // Insert raw JSON to preserve duplicate fields inside the retired
            // value: IgnoredAny must skip it without validating the old model.
            let mut legacy = serde_json::to_string(&wire).unwrap();
            if let Some(retired) = retired {
                let settings = if version == 1 {
                    &wire["applied"]
                } else {
                    &wire["profiles"][1]["settings"]
                };
                let settings = serde_json::to_string(settings).unwrap();
                let with_retired = format!(
                    "{},\"filters\":{retired}}}",
                    settings.strip_suffix('}').unwrap()
                );
                legacy = legacy.replacen(&settings, &with_retired, 1);
            }
            fs::write(&path, &legacy).unwrap();
            let (mut store, outcome) = SettingsStore::load(path.clone());
            let LoadOutcome::Loaded {
                document,
                retired_filters_present,
            } = outcome
            else {
                panic!("v{version} {retired:?} must load: {outcome:?}")
            };
            assert_eq!(retired_filters_present, retired.is_some());
            assert_eq!(*document, expected);
            assert_eq!(store.snapshot(), Some(&expected));
            assert_eq!(fs::read(&path).unwrap(), legacy.as_bytes());
            assert_no_temp_files(&directory);

            store
                .save_profiles(
                    &document.profiles,
                    document.last_source.as_ref(),
                    document.preferences.clone(),
                )
                .unwrap();
            let written = fs::read(&path).unwrap();
            let wire: serde_json::Value = serde_json::from_slice(&written).unwrap();
            assert_eq!(wire["schema_version"], 3);
            for profile in wire["profiles"].as_array().unwrap() {
                assert!(profile["settings"].get("filters").is_none());
            }
            let (_, outcome) = SettingsStore::load(path.clone());
            let LoadOutcome::Loaded {
                document,
                retired_filters_present,
            } = outcome
            else {
                panic!("v3 migration must reload")
            };
            assert!(!retired_filters_present);
            assert_eq!(*document, expected);
            // An old strict build cannot accept the new discriminator, and
            // reverting the executable cannot recover discarded filter bytes.
            assert_ne!(wire["schema_version"], version);
        }
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn retired_key_duplicates_current_keys_and_malformed_json_refuse_without_partial_recovery() {
    let directory = temp_dir("retired-key-refusal");
    let path = settings_path(&directory);
    let inner = APPLIED.strip_suffix('}').unwrap();
    for version in [1, 2, 3] {
        let mut cases = vec![
            format!(r#"{inner},"filters":null,"filters":false}}"#),
            format!(r#"{inner},"filters":null,"future":true}}"#),
            format!(r#"{inner},"filters":null,"renderer":null}}"#),
            format!(r#"{inner},"filters":null,"audio":null}}"#),
        ];
        if version == 3 {
            for retired in ["null", RETIRED_FILTERS, "42", "[]"] {
                cases.push(applied_with_filters(retired));
            }
        }
        for settings in cases {
            let json = if version == 1 {
                document_with_applied(&settings)
            } else {
                v2_document(&format!("[{}]", wire_profile("serial", &settings)), "null").replacen(
                    r#""schema_version":2"#,
                    &format!(r#""schema_version":{version}"#),
                    1,
                )
            };
            let (mut store, outcome) = refused_schema(&directory, &json, &json);
            assert!(matches!(
                outcome,
                LoadOutcome::Refused(SettingsLoadError::Schema { .. })
            ));
            assert!(store.snapshot().is_none());
            assert!(matches!(
                persist_globals(&mut store, manual_output_preferences()),
                Err(SettingsWriteError::RefusedOriginal { .. })
            ));
            assert_eq!(fs::read(&path).unwrap(), json.as_bytes());
            store.reset(preferences()).unwrap();
            assert!(!store.is_refused());
            let (_, outcome) = SettingsStore::load(path.clone());
            assert!(matches!(
                outcome,
                LoadOutcome::Loaded {
                    retired_filters_present: false,
                    ..
                }
            ));
        }
        let malformed = format!(r#"{inner},"filters":{{"entries":[}}}}"#);
        let json = if version == 1 {
            document_with_applied(&malformed)
        } else {
            v2_document(&format!("[{}]", wire_profile("serial", &malformed)), "null").replacen(
                r#""schema_version":2"#,
                &format!(r#""schema_version":{version}"#),
                1,
            )
        };
        fs::write(&path, &json).unwrap();
        let (mut store, outcome) = SettingsStore::load(path.clone());
        assert!(matches!(
            outcome,
            LoadOutcome::Refused(SettingsLoadError::MalformedJson { .. })
        ));
        assert!(matches!(
            persist_globals(&mut store, preferences()),
            Err(SettingsWriteError::RefusedOriginal { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), json.as_bytes());
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn lazy_migration_rename_fault_preserves_legacy_bytes_and_the_full_renderer() {
    let directory = temp_dir("retired-migration-fault");
    let path = settings_path(&directory);
    let legacy = document_with_applied(&applied_with_filters(RETIRED_FILTERS));
    fs::write(&path, &legacy).unwrap();
    let mut store = SettingsStore::with_injected_fault(path.clone(), WriteFaultPoint::BeforeRename);
    let previous = store.snapshot().unwrap().clone();
    let target = draft_with_renderer(reference_renderer());
    let applied = applied_settings(target.clone());
    assert!(matches!(
        persist_verified(&mut store, &applied, preferences()),
        Err(SettingsWriteError::Rename { .. })
    ));
    assert_eq!(store.snapshot(), Some(&previous));
    assert_eq!(fs::read(&path).unwrap(), legacy.as_bytes());
    assert_no_temp_files(&directory);
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document,
        retired_filters_present,
    } = outcome
    else {
        panic!("legacy original must remain loadable")
    };
    assert!(retired_filters_present);
    assert_eq!(*document, previous);
    assert!(
        persist_verified(&mut store, &applied, preferences())
            .unwrap()
            .is_confirmed()
    );
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded {
        document,
        retired_filters_present,
    } = outcome
    else {
        panic!("committed renderer must load")
    };
    assert!(!retired_filters_present);
    assert_eq!(last_settings(&document), Some(&target));
    fs::remove_dir_all(directory).unwrap();
}

// ---------------------------------------------------------------------------
// Renderer persistence (FUR-016)
// ---------------------------------------------------------------------------

/// The complete non-baseline renderer pinned by the product reference fixture.
const REFERENCE_RENDERER_JSON: &str =
    include_str!("../../tests/fixtures/FUR-016/reference-renderer.json");

fn reference_renderer_json() -> &'static str {
    REFERENCE_RENDERER_JSON.trim()
}

/// The reference fixture rebuilt through the validating domain constructor.
fn reference_renderer() -> RendererSettings {
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

/// Inject a `renderer` member into the object-form `APPLIED` fixture.
fn applied_with_renderer(renderer_json: &str) -> String {
    let inner = APPLIED
        .strip_suffix('}')
        .expect("APPLIED fixture is an object");
    format!(r#"{inner},"renderer":{renderer_json}}}"#)
}

fn draft_with_renderer(renderer: RendererSettings) -> DraftSettings {
    DraftSettings {
        renderer,
        ..draft_settings()
    }
}

#[test]
fn pre_renderer_v1_document_migrates_preserving_baseline() {
    let directory = temp_dir("renderer-parity");
    let path = settings_path(&directory);
    let original = canonical_document();
    fs::write(&path, &original).unwrap();

    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("pre-renderer v1 document must load, got {outcome:?}");
    };
    let loaded = last_settings(&document).expect("applied present");
    assert_eq!(loaded.renderer, RendererSettings::default());
    assert!(loaded.renderer.is_baseline());
    assert_eq!(fs::read_to_string(&path).unwrap(), original);

    persist_verified(
        &mut store,
        &applied_settings(draft_settings()),
        preferences(),
    )
    .unwrap();

    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: migrated, ..
    } = outcome
    else {
        panic!("migrated profile must load")
    };
    assert_eq!(last_settings(&migrated), last_settings(&document));
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn reference_renderer_round_trips_exactly_through_the_store() {
    let directory = temp_dir("renderer-roundtrip");
    let path = settings_path(&directory);
    let reference = reference_renderer_json();
    fs::write(
        &path,
        document_with_applied(&applied_with_renderer(reference)),
    )
    .unwrap();

    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("reference renderer document must load, got {outcome:?}");
    };
    let loaded = last_settings(&document).expect("applied present");
    assert_eq!(loaded.renderer, reference_renderer());
    assert!(!loaded.renderer.is_baseline());

    let applied = applied_settings(draft_with_renderer(reference_renderer()));
    persist_verified(&mut store, &applied, preferences()).unwrap();

    let written = fs::read_to_string(&path).unwrap();
    let expected = serde_json::to_string(&reference_renderer()).unwrap();
    assert!(
        written.contains(&format!(r#""renderer":{expected}"#)),
        "a non-baseline renderer is persisted in full: {written}"
    );
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded {
        document: reloaded, ..
    } = outcome
    else {
        panic!("re-encoded reference must load");
    };
    assert_eq!(
        last_settings(&reloaded).unwrap().renderer,
        reference_renderer()
    );
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn malformed_renderer_json_refuses_the_complete_original_and_blocks_saves_until_reset() {
    let base = concat!(
        r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0.0,"#,
        r#""dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false}"#,
    );
    let duplicate_renderer_key = {
        let inner = APPLIED
            .strip_suffix('}')
            .expect("APPLIED fixture is an object");
        let r = reference_renderer_json();
        let applied = format!(r#"{inner},"renderer":{r},"renderer":{r}}}"#);
        format!(r#"{{"schema_version":1,"applied":{applied},"preferences":{PREFS}}}"#)
    };
    let cases: Vec<(&str, String)> = vec![
        (
            "renderer null",
            document_with_applied(&applied_with_renderer("null")),
        ),
        (
            "renderer array",
            document_with_applied(&applied_with_renderer("[]")),
        ),
        (
            "renderer string",
            document_with_applied(&applied_with_renderer("\"lanczos\"")),
        ),
        (
            "renderer number",
            document_with_applied(&applied_with_renderer("1")),
        ),
        (
            "renderer missing field",
            document_with_applied(&applied_with_renderer(
                r#"{"scale":"lanczos","cscale":"auto"}"#,
            )),
        ),
        (
            "renderer empty object",
            document_with_applied(&applied_with_renderer("{}")),
        ),
        (
            "renderer unknown field",
            document_with_applied(&applied_with_renderer(
                r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0.0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false,"extra":1}"#,
            )),
        ),
        (
            "scale auto refused",
            document_with_applied(&applied_with_renderer(
                &base.replace(r#""scale":"lanczos""#, r#""scale":"auto""#),
            )),
        ),
        (
            "unknown kernel token",
            document_with_applied(&applied_with_renderer(
                &base.replace(r#""scale":"lanczos""#, r#""scale":"sharpen""#),
            )),
        ),
        (
            "non-string kernel",
            document_with_applied(&applied_with_renderer(
                &base.replace(r#""scale":"lanczos""#, r#""scale":1"#),
            )),
        ),
        (
            "null kernel",
            document_with_applied(&applied_with_renderer(
                &base.replace(r#""scale":"lanczos""#, r#""scale":null"#),
            )),
        ),
        (
            "antiring out of range",
            document_with_applied(&applied_with_renderer(
                &base.replace(r#""scale_antiring":0.0"#, r#""scale_antiring":1.5"#),
            )),
        ),
        (
            "iterations out of range",
            document_with_applied(&applied_with_renderer(&base.replace(
                r#""enabled":false,"iterations":1"#,
                r#""enabled":false,"iterations":17"#,
            ))),
        ),
        (
            "grain out of range",
            document_with_applied(&applied_with_renderer(&base.replace(
                r#""iterations":1,"grain":32"#,
                r#""iterations":1,"grain":4097"#,
            ))),
        ),
        (
            "disabled deband still validated",
            document_with_applied(&applied_with_renderer(&base.replace(
                r#""enabled":false,"iterations":1,"grain":32"#,
                r#""enabled":false,"iterations":1,"grain":5000"#,
            ))),
        ),
        (
            "deband positional array",
            document_with_applied(&applied_with_renderer(&base.replace(
                r#"{"enabled":false,"iterations":1,"grain":32}"#,
                r#"[false,1,32]"#,
            ))),
        ),
        (
            "deband unknown field",
            document_with_applied(&applied_with_renderer(&base.replace(
                r#"{"enabled":false,"iterations":1,"grain":32}"#,
                r#"{"enabled":false,"iterations":1,"grain":32,"extra":1}"#,
            ))),
        ),
        (
            "deband missing field",
            document_with_applied(&applied_with_renderer(&base.replace(
                r#"{"enabled":false,"iterations":1,"grain":32}"#,
                r#"{"enabled":false}"#,
            ))),
        ),
        (
            "interpolation non-bool",
            document_with_applied(&applied_with_renderer(
                &base.replace(r#""interpolation":false"#, r#""interpolation":"no""#),
            )),
        ),
        (
            "unknown dither",
            document_with_applied(&applied_with_renderer(
                &base.replace(r#""dither":"fruit""#, r#""dither":"floyd""#),
            )),
        ),
        (
            "duplicate scale",
            document_with_applied(&applied_with_renderer(&base.replace(
                r#""scale":"lanczos""#,
                r#""scale":"lanczos","scale":"mitchell""#,
            ))),
        ),
        (
            "duplicate scale order-reversed",
            document_with_applied(&applied_with_renderer(&base.replace(
                r#""interpolation":false}"#,
                r#""interpolation":false,"scale":"lanczos","scale":"mitchell"}"#,
            ))),
        ),
        ("duplicate renderer key", duplicate_renderer_key),
    ];

    let directory = temp_dir("renderer-violations");
    let path = settings_path(&directory);
    let applied = applied_settings(draft_with_renderer(reference_renderer()));
    for (label, json) in cases {
        fs::write(&path, &json).unwrap();
        let original = fs::read(&path).unwrap();

        let (mut store, outcome) = SettingsStore::load(path.clone());
        assert!(
            matches!(
                outcome,
                LoadOutcome::Refused(SettingsLoadError::Schema { .. })
            ),
            "{label}: expected schema refusal, got {outcome:?}"
        );
        assert!(store.is_refused(), "{label}");
        assert!(store.snapshot().is_none(), "{label}");
        assert!(
            matches!(
                persist_verified(&mut store, &applied, preferences()),
                Err(SettingsWriteError::RefusedOriginal { .. })
            ),
            "{label}: applied write must stay blocked"
        );
        assert!(
            matches!(
                persist_globals(&mut store, preferences()),
                Err(SettingsWriteError::RefusedOriginal { .. })
            ),
            "{label}: preference write must stay blocked"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "{label}: the complete original must be preserved"
        );
        assert_no_temp_files(&directory);

        // Only an explicit reset clears the refusal.
        store.reset(preferences()).unwrap();
        assert!(!store.is_refused(), "{label}");
        let (_, outcome) = SettingsStore::load(path.clone());
        assert!(
            matches!(outcome, LoadOutcome::Loaded { .. }),
            "{label}: reset must produce a loadable document"
        );
    }
    fs::remove_dir_all(&directory).unwrap();
}

// ---------------------------------------------------------------------------
// Automatic source profiles: identity, strict v2 migration and complete saves
// ---------------------------------------------------------------------------

fn profile_identity(port: u8, serial: Option<&str>) -> DeviceIdentity {
    DeviceIdentity::new(
        0x1234,
        0x5678,
        UsbTopology::new(
            "usb-0000:03:00.0".to_owned(),
            vec![NonZeroU8::new(port).unwrap(), NonZeroU8::new(1).unwrap()],
        )
        .unwrap(),
        serial.map(str::to_owned),
    )
    .unwrap()
}

fn two_profile_settings() -> (DraftSettings, DraftSettings) {
    let a = DraftSettings {
        video: auto_video_request(),
        renderer: reference_renderer(),
        ..draft_settings()
    };
    let mut b = DraftSettings {
        video: video_request(u32::from_le_bytes(*b"MJPG"), 1280, 720, 30000, 1001),
        audio: AudioSelection::Disabled {
            retained: Some(
                AudioSourceIdentity::new(
                    "alsa_input.usb-B".to_owned(),
                    vec![
                        ("card.id".to_owned(), "4".to_owned()),
                        ("node.name".to_owned(), "capture.b".to_owned()),
                    ],
                )
                .unwrap(),
            ),
        },
        renderer: RendererSettings::default(),
    };
    b.video.identity = profile_identity(3, Some("B"));
    (a, b)
}

#[test]
fn source_profiles_round_trip_all_settings_and_globals_without_cross_source_inheritance() {
    let directory = temp_dir("multi-profile");
    let path = settings_path(&directory);
    let (a, b) = two_profile_settings();
    let mut profiles = SourceProfiles::default();
    profiles.upsert_verified(&applied_settings(a.clone()), ProfileBinding::Serial);
    profiles.upsert_verified(&applied_settings(b.clone()), ProfileBinding::Serial);
    let globals = LocalPreferences {
        gain: PlaybackGain::new(0, true).unwrap(),
        fullscreen: true,
        output: PersistentOutputChoice::Manual(manual_sink_identity()),
        stats_hidden: true,
        logging_enabled: true,
    };
    let (mut store, _) = SettingsStore::load(path.clone());
    store
        .save_profiles(&profiles, Some(&b.video.identity), globals.clone())
        .unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("all profiles must reload")
    };
    assert_eq!(document.profiles.entries().len(), 2);
    assert_eq!(document.profiles.lookup(&a.video.identity, true), Some(&a));
    assert_eq!(document.profiles.lookup(&b.video.identity, true), Some(&b));
    assert_eq!(last_settings(&document), Some(&b));
    assert_eq!(document.preferences, globals);
    assert!(
        document
            .profiles
            .lookup(&profile_identity(4, Some("C")), true)
            .is_none()
    );
    assert!(!fs::read_to_string(path).unwrap().contains("\"applied\""));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn unique_serial_port_move_replaces_one_profile_and_last_source_only_after_verified_save() {
    let directory = temp_dir("serial-port-move");
    let path = settings_path(&directory);
    let settings = draft_with_renderer(reference_renderer());
    let old = settings.video.identity.clone();
    let moved = profile_identity(8, Some("SER42"));
    let mut profiles = SourceProfiles::default();
    profiles.upsert_verified(&applied_settings(settings.clone()), ProfileBinding::Serial);
    profiles.observe(std::iter::once(&moved));
    assert_eq!(profiles.lookup(&moved, true), Some(&settings));
    assert_eq!(profiles.entries()[0].settings().video.identity, old);
    let (mut store, _) = SettingsStore::load(path.clone());
    store
        .save_profiles(&profiles, Some(&old), preferences())
        .unwrap();
    let mut moved_settings = settings.clone();
    moved_settings.video.identity = moved.clone();
    profiles.upsert_verified(
        &applied_settings(moved_settings.clone()),
        ProfileBinding::Serial,
    );
    assert_eq!(profiles.entries().len(), 1);
    assert_eq!(profiles.entries()[0].settings(), &moved_settings);
    assert_eq!(store.snapshot().unwrap().last_source.as_ref(), Some(&old));
    store
        .save_profiles(&profiles, Some(&moved), preferences())
        .unwrap();
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("moved profile must reload")
    };
    assert_eq!(document.profiles.entries().len(), 1);
    assert_eq!(document.last_source.as_ref(), Some(&moved));
    assert_eq!(last_settings(&document), Some(&moved_settings));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn exact_raw_serial_and_vid_pid_are_not_normalized_or_cross_matched() {
    let settings = draft_settings();
    let mut profiles = SourceProfiles::default();
    profiles.upsert_verified(&applied_settings(settings.clone()), ProfileBinding::Serial);
    for serial in ["ser42", "SER42 ", "usb-SER42"] {
        assert!(
            profiles
                .lookup(&profile_identity(2, Some(serial)), true)
                .is_none()
        );
    }
    let other_product =
        DeviceIdentity::new(0x1234, 0x5679, topology(), Some("SER42".to_owned())).unwrap();
    assert!(profiles.lookup(&other_product, true).is_none());
    let other_vendor =
        DeviceIdentity::new(0x1235, 0x5678, topology(), Some("SER42".to_owned())).unwrap();
    assert!(profiles.lookup(&other_vendor, true).is_none());
    profiles.observe([&settings.video.identity, &other_product, &other_vendor].into_iter());
    assert_eq!(
        profiles.binding_for(&settings.video.identity, true),
        ProfileBinding::Serial
    );
}

#[test]
fn serialless_profiles_remain_exact_topology_bound_when_devices_disappear_or_move() {
    let mut a = draft_settings();
    a.video.identity = profile_identity(2, None);
    let mut b = a.clone();
    b.video.identity = profile_identity(3, None);
    b.video.mode.rate = FrameRate::new(30000, 1001).unwrap();
    let mut profiles = SourceProfiles::default();
    profiles.upsert_verified(&applied_settings(a.clone()), ProfileBinding::Serial);
    profiles.upsert_verified(&applied_settings(b.clone()), ProfileBinding::Topology);
    profiles.observe([&a.video.identity, &b.video.identity].into_iter());
    assert_eq!(profiles.entries().len(), 2);
    assert_eq!(
        profiles.binding_for(&a.video.identity, true),
        ProfileBinding::Topology
    );
    assert_eq!(profiles.lookup(&a.video.identity, false), Some(&a));
    assert_eq!(profiles.lookup(&b.video.identity, false), Some(&b));
    profiles.observe(std::iter::once(&a.video.identity));
    assert!(profiles.lookup(&profile_identity(8, None), true).is_none());
}

#[test]
fn duplicate_serial_observation_is_sticky_without_b_success_and_survives_global_save_reload() {
    let directory = temp_dir("sticky-collision");
    let path = settings_path(&directory);
    let settings = draft_with_renderer(reference_renderer());
    let a = settings.video.identity.clone();
    let b = profile_identity(3, Some("SER42"));
    let mut profiles = SourceProfiles::default();
    profiles.upsert_verified(&applied_settings(settings.clone()), ProfileBinding::Serial);
    profiles.observe([&a, &a].into_iter());
    assert_eq!(profiles.binding_for(&a, true), ProfileBinding::Serial);
    profiles.observe([&a, &b].into_iter());
    assert_eq!(
        profiles.entries().len(),
        1,
        "observation creates no fake B profile"
    );
    assert_eq!(profiles.entries()[0].binding(), ProfileBinding::Topology);
    assert_eq!(profiles.entries()[0].settings(), &settings);
    profiles.observe(std::iter::once(&a));
    assert_eq!(profiles.binding_for(&a, true), ProfileBinding::Topology);
    assert!(profiles.lookup(&b, true).is_none());
    let (mut store, _) = SettingsStore::load(path.clone());
    store
        .save_profiles(&profiles, Some(&a), manual_output_preferences())
        .unwrap();
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { mut document, .. } = outcome else {
        panic!("collision profile must reload")
    };
    document.profiles.observe(std::iter::once(&a));
    assert_eq!(
        document.profiles.binding_for(&a, true),
        ProfileBinding::Topology
    );
    assert_eq!(document.profiles.lookup(&a, true), Some(&settings));
    assert!(document.profiles.lookup(&b, true).is_none());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn collision_without_profiles_restricts_first_success_and_a_pre_collision_serial_receipt() {
    let a = profile_identity(2, Some("DUP"));
    let b = profile_identity(3, Some("DUP"));
    let mut profiles = SourceProfiles::default();
    profiles.observe([&a, &b].into_iter());
    assert!(profiles.entries().is_empty());
    profiles.observe(std::iter::once(&a));
    assert_eq!(profiles.binding_for(&a, true), ProfileBinding::Topology);
    let mut a_settings = draft_settings();
    a_settings.video.identity = a.clone();
    profiles.upsert_verified(
        &applied_settings(a_settings.clone()),
        ProfileBinding::Serial,
    );
    let mut b_settings = a_settings.clone();
    b_settings.video.identity = b.clone();
    b_settings.video.mode.rate = FrameRate::new(30000, 1001).unwrap();
    profiles.upsert_verified(
        &applied_settings(b_settings.clone()),
        ProfileBinding::Serial,
    );
    assert_eq!(profiles.entries().len(), 2);
    assert!(
        profiles
            .entries()
            .iter()
            .all(|profile| profile.binding() == ProfileBinding::Topology)
    );
    assert_eq!(profiles.lookup(&a, true), Some(&a_settings));
    assert_eq!(profiles.lookup(&b, true), Some(&b_settings));
}

#[test]
fn v1_migration_is_lazy_and_the_first_global_save_preserves_full_profile_and_preferences() {
    let directory = temp_dir("v1-lazy-migration");
    let path = settings_path(&directory);
    let payload = applied_with_filters(RETIRED_FILTERS);
    let payload = format!(
        "{},\"renderer\":{}}}",
        payload.strip_suffix('}').unwrap(),
        reference_renderer_json(),
    );
    let legacy = document_with_applied(&payload);
    fs::write(&path, legacy.as_bytes()).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("complete v1 payload must migrate")
    };
    let expected = DraftSettings {
        renderer: reference_renderer(),
        ..draft_settings()
    };
    assert_eq!(last_settings(&document), Some(&expected));
    assert_eq!(
        document.profiles.entries()[0].binding(),
        ProfileBinding::Serial
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        legacy.as_bytes(),
        "load must not rewrite v1"
    );
    let globals = manual_output_preferences();
    store
        .save_profiles(
            &document.profiles,
            document.last_source.as_ref(),
            globals.clone(),
        )
        .unwrap();
    let bytes = fs::read(&path).unwrap();
    let wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(wire["schema_version"], 3);
    assert!(wire.get("applied").is_none());
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded {
        document: migrated, ..
    } = outcome
    else {
        panic!("lazy v3 save must reload")
    };
    assert_eq!(last_settings(&migrated), Some(&expected));
    assert_eq!(migrated.last_source, document.last_source);
    assert_eq!(migrated.preferences, globals);
    fs::remove_dir_all(directory).unwrap();
}

fn v2_document(profiles: &str, last_source: &str) -> String {
    format!(
        r#"{{"schema_version":2,"profiles":{profiles},"last_source":{last_source},"preferences":{PREFS}}}"#
    )
}

fn wire_profile(binding: &str, settings: &str) -> String {
    format!(r#"{{"binding":"{binding}","settings":{settings}}}"#)
}

#[test]
fn strict_v2_and_v3_refuse_invalid_collection_pointer_duplicates_nulls_and_settings_without_touching_bytes()
 {
    let directory = temp_dir("v2-refusals");
    let serial = wire_profile("serial", APPLIED);
    let topology = wire_profile("topology", APPLIED);
    let moved = APPLIED.replace("\"ports\":[2,1]", "\"ports\":[3,1]");
    let moved_serial = wire_profile("serial", &moved);
    let moved_topology = wire_profile("topology", &moved);
    let bad_pointer = serde_json::to_string(&profile_identity(9, Some("SER42"))).unwrap();
    let cases = [
        ("null profiles", v2_document("null", "null")),
        ("null profile", v2_document("[null]", "null")),
        (
            "positional profile",
            v2_document(&format!(r#"[["serial",{APPLIED}]]"#), "null"),
        ),
        (
            "missing binding",
            v2_document(&format!(r#"[{{"settings":{APPLIED}}}]"#), "null"),
        ),
        (
            "missing settings",
            v2_document(r#"[{"binding":"serial"}]"#, "null"),
        ),
        (
            "null binding",
            v2_document(
                &format!(r#"[{{"binding":null,"settings":{APPLIED}}}]"#),
                "null",
            ),
        ),
        (
            "invalid binding",
            v2_document(&format!("[{}]", wire_profile("auto", APPLIED)), "null"),
        ),
        (
            "null settings",
            v2_document(r#"[{"binding":"serial","settings":null}]"#, "null"),
        ),
        (
            "unknown profile field",
            v2_document(
                &format!(r#"[{{"binding":"serial","settings":{APPLIED},"extra":0}}]"#),
                "null",
            ),
        ),
        (
            "duplicate binding",
            v2_document(
                &format!(r#"[{{"binding":"serial","binding":"topology","settings":{APPLIED}}}]"#),
                "null",
            ),
        ),
        (
            "duplicate settings",
            v2_document(
                &format!(r#"[{{"binding":"serial","settings":{APPLIED},"settings":{APPLIED}}}]"#),
                "null",
            ),
        ),
        (
            "duplicate full identity",
            v2_document(&format!("[{serial},{serial}]"), "null"),
        ),
        (
            "multiple serial group",
            v2_document(&format!("[{serial},{moved_serial}]"), "null"),
        ),
        (
            "mixed bindings",
            v2_document(&format!("[{serial},{moved_topology}]"), "null"),
        ),
        (
            "reverse mixed bindings",
            v2_document(&format!("[{topology},{moved_serial}]"), "null"),
        ),
        (
            "serial binding without serial",
            v2_document(
                &format!(
                    "[{}]",
                    wire_profile(
                        "serial",
                        &APPLIED.replace(r#""serial":"SER42""#, r#""serial":null"#)
                    )
                ),
                "null",
            ),
        ),
        (
            "last pointer missing",
            format!(r#"{{"schema_version":2,"profiles":[],"preferences":{PREFS}}}"#),
        ),
        (
            "last pointer to absent identity",
            v2_document(&format!("[{serial}]"), &bad_pointer),
        ),
        (
            "last pointer on empty profiles",
            v2_document("[]", &bad_pointer),
        ),
        (
            "duplicate last pointer",
            format!(
                r#"{{"schema_version":2,"profiles":[],"last_source":null,"last_source":null,"preferences":{PREFS}}}"#
            ),
        ),
        (
            "duplicate null profiles",
            format!(
                r#"{{"schema_version":2,"profiles":null,"profiles":[],"last_source":null,"preferences":{PREFS}}}"#
            ),
        ),
        (
            "missing profiles",
            format!(r#"{{"schema_version":2,"last_source":null,"preferences":{PREFS}}}"#),
        ),
        (
            "legacy payload key",
            format!(
                r#"{{"schema_version":2,"profiles":[],"last_source":null,"applied":null,"preferences":{PREFS}}}"#
            ),
        ),
        (
            "invalid exact rate",
            v2_document(
                &format!(
                    "[{}]",
                    wire_profile(
                        "serial",
                        &APPLIED.replace("\"denominator\":1001", "\"denominator\":0")
                    )
                ),
                "null",
            ),
        ),
        (
            "invalid identity",
            v2_document(
                &format!(
                    "[{}]",
                    wire_profile(
                        "serial",
                        &APPLIED.replace("\"ports\":[2,1]", "\"ports\":[]")
                    )
                ),
                "null",
            ),
        ),
        (
            "null renderer",
            v2_document(
                &format!(
                    "[{}]",
                    wire_profile("serial", &applied_with_renderer("null"))
                ),
                "null",
            ),
        ),
        (
            "duplicate nested rate",
            v2_document(
                &format!(
                    "[{}]",
                    wire_profile(
                        "serial",
                        &APPLIED.replace(
                            "\"denominator\":1001",
                            "\"denominator\":1001,\"denominator\":1001"
                        )
                    )
                ),
                "null",
            ),
        ),
        (
            "duplicate schema version",
            format!(
                r#"{{"schema_version":2,"schema_version":2,"profiles":[],"last_source":null,"preferences":{PREFS}}}"#
            ),
        ),
    ];
    for version in [2, 3] {
        for (label, json) in &cases {
            let json = json.replace(
                r#""schema_version":2"#,
                &format!(r#""schema_version":{version}"#),
            );
            let (mut store, _) = refused_schema(&directory, label, &json);
            assert!(
                matches!(
                    store.save_profiles(&SourceProfiles::default(), None, preferences()),
                    Err(SettingsWriteError::RefusedOriginal { .. })
                ),
                "v{version}: {label}"
            );
            assert_eq!(fs::read(store.path()).unwrap(), json.as_bytes(), "{label}");
            assert_no_temp_files(&directory);
        }
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn topology_duplicates_of_one_serial_group_are_legal_and_never_promoted_after_reload() {
    let directory = temp_dir("v2-topology-group");
    let path = settings_path(&directory);
    let moved = APPLIED.replace("\"ports\":[2,1]", "\"ports\":[3,1]");
    let original = v2_document(
        &format!(
            "[{},{}]",
            wire_profile("topology", APPLIED),
            wire_profile("topology", &moved)
        ),
        "null",
    );
    fs::write(&path, original.as_bytes()).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { mut document, .. } = outcome else {
        panic!("topology group must load")
    };
    assert_eq!(document.profiles.entries().len(), 2);
    document.profiles.observe(std::iter::once(&identity()));
    assert_eq!(
        document.profiles.binding_for(&identity(), true),
        ProfileBinding::Topology
    );
    assert_eq!(
        document.profiles.lookup(&identity(), true),
        Some(&draft_settings())
    );
    store
        .save_profiles(&document.profiles, None, preferences())
        .unwrap();
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded {
        document: reloaded, ..
    } = outcome
    else {
        panic!("topology group must survive save")
    };
    assert_eq!(document.profiles, reloaded.profiles);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn preference_save_and_precommit_failure_preserve_all_profiles_and_the_committed_last_source() {
    let directory = temp_dir("collection-write-fault");
    let path = settings_path(&directory);
    let (a, b) = two_profile_settings();
    let mut profiles = SourceProfiles::default();
    profiles.upsert_verified(&applied_settings(a.clone()), ProfileBinding::Serial);
    profiles.upsert_verified(&applied_settings(b.clone()), ProfileBinding::Serial);
    let (mut store, _) = SettingsStore::load(path.clone());
    store
        .save_profiles(&profiles, Some(&b.video.identity), preferences())
        .unwrap();
    let globals_before = manual_output_preferences();
    store
        .save_profiles(&profiles, Some(&b.video.identity), globals_before.clone())
        .unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded {
        document: global_save,
        ..
    } = outcome
    else {
        panic!("global save must load")
    };
    assert_eq!(
        global_save.profiles.lookup(&a.video.identity, true),
        Some(&a)
    );
    assert_eq!(
        global_save.profiles.lookup(&b.video.identity, true),
        Some(&b)
    );
    assert_eq!(global_save.last_source.as_ref(), Some(&b.video.identity));
    assert_eq!(global_save.preferences, globals_before);
    let original = fs::read(&path).unwrap();
    let committed = store.snapshot().unwrap().clone();
    let mut faulted =
        SettingsStore::with_injected_fault(path.clone(), WriteFaultPoint::BeforeRename);
    let mut newer_a = a.clone();
    newer_a.video.mode.rate = FrameRate::new(30000, 1001).unwrap();
    profiles.upsert_verified(&applied_settings(newer_a.clone()), ProfileBinding::Serial);
    assert!(matches!(
        faulted.save_profiles(
            &profiles,
            Some(&a.video.identity),
            manual_output_preferences()
        ),
        Err(SettingsWriteError::Rename { .. })
    ));
    assert_eq!(faulted.snapshot(), Some(&committed));
    assert_eq!(fs::read(&path).unwrap(), original);
    let globals = manual_output_preferences();
    faulted
        .save_profiles(&profiles, Some(&a.video.identity), globals.clone())
        .unwrap();
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("whole-collection retry must load")
    };
    assert_eq!(
        document.profiles.lookup(&a.video.identity, true),
        Some(&newer_a)
    );
    assert_eq!(document.profiles.lookup(&b.video.identity, true), Some(&b));
    assert_eq!(document.preferences, globals);
    assert_eq!(document.last_source.as_ref(), Some(&a.video.identity));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn invalid_last_source_is_a_typed_prewrite_refusal_and_reset_discards_collision_history() {
    let directory = temp_dir("profile-pointer-write");
    let path = settings_path(&directory);
    let mut profiles = SourceProfiles::default();
    let a = identity();
    let b = profile_identity(3, Some("SER42"));
    profiles.observe([&a, &b].into_iter());
    profiles.upsert_verified(&applied_settings(draft_settings()), ProfileBinding::Serial);
    let (mut store, _) = SettingsStore::load(path.clone());
    store
        .save_profiles(&profiles, Some(&a), preferences())
        .unwrap();
    let original = fs::read(&path).unwrap();
    let snapshot = store.snapshot().unwrap().clone();
    assert!(matches!(
        store.save_profiles(&profiles, Some(&b), manual_output_preferences()),
        Err(SettingsWriteError::InvalidValue {
            source: SettingsValidationError::Profiles(ProfileValidationError::InvalidLastSource),
        })
    ));
    assert_eq!(store.snapshot(), Some(&snapshot));
    assert_eq!(fs::read(&path).unwrap(), original);
    let globals = manual_output_preferences();
    store.reset(globals.clone()).unwrap();
    let cleared = store.snapshot().unwrap();
    assert!(cleared.profiles.entries().is_empty());
    assert_eq!(
        cleared.profiles.binding_for(&a, true),
        ProfileBinding::Serial
    );
    assert!(cleared.last_source.is_none());
    assert_eq!(cleared.preferences, globals);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn v1_preferences_only_migrates_lazily_with_empty_profiles_and_unchanged_globals() {
    let directory = temp_dir("v1-global-only");
    let path = settings_path(&directory);
    let original = document_with_applied("null");
    fs::write(&path, original.as_bytes()).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded { document, .. } = outcome else {
        panic!("v1 globals must load")
    };
    assert!(document.profiles.entries().is_empty());
    assert!(document.last_source.is_none());
    assert_eq!(document.preferences, preferences());
    assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
    store
        .save_profiles(&document.profiles, None, document.preferences.clone())
        .unwrap();
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded {
        document: migrated, ..
    } = outcome
    else {
        panic!("migrated globals must load")
    };
    assert_eq!(*migrated, *document);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn encode_invalid_profile_settings_refuses_before_writing_and_keeps_the_committed_document() {
    let directory = temp_dir("invalid-profile-write");
    let path = settings_path(&directory);
    let (mut store, _) = SettingsStore::load(path.clone());
    let mut valid = SourceProfiles::default();
    valid.upsert_verified(&applied_settings(draft_settings()), ProfileBinding::Serial);
    store
        .save_profiles(&valid, Some(&identity()), preferences())
        .unwrap();
    let original = fs::read(&path).unwrap();
    let committed = store.snapshot().unwrap().clone();
    let mut invalid = draft_settings();
    invalid.video.resolved_fourcc = None;
    let invalid = SourceProfiles::new(vec![
        SourceProfile::new(ProfileBinding::Serial, invalid).unwrap(),
    ])
    .unwrap();
    assert!(matches!(
        store.save_profiles(&invalid, Some(&identity()), manual_output_preferences()),
        Err(SettingsWriteError::InvalidValue {
            source: SettingsValidationError::UnresolvedVideo
        })
    ));
    assert_eq!(store.snapshot(), Some(&committed));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_no_temp_files(&directory);
    fs::remove_dir_all(directory).unwrap();
}
