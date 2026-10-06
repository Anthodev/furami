//! FUR-012 settings store behavior regressions: every load refusal class,
//! exact-FPS roundtrip, atomic-writer fault survival, explicit reset and
//! post-commit durability uncertainty.

use std::fs;
use std::num::NonZeroU8;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use furami::domain::capture::{
    AudioSelection, AudioSourceIdentity, CaptureMode, CapturedFourCc, DeviceIdentity, FrameRate,
    FrameSize, ModeRequest, PlaybackGain, UsbTopology,
};
use furami::domain::state::{AppliedSettings, DraftSettings, ModelEffect, ProductModel};
use furami::settings::{
    Durability, LoadOutcome, LocalPreferences, SettingsLoadError, SettingsStore,
    SettingsWriteError, WriteFaultPoint, WriteOutcome,
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

fn mode(fourcc: u32, width: u32, height: u32, numerator: u32, denominator: u32) -> CaptureMode {
    CaptureMode {
        captured_fourcc: CapturedFourCc::from_kernel(fourcc),
        size: FrameSize::new(width, height).unwrap(),
        rate: FrameRate::new(numerator, denominator).unwrap(),
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
        video: ModeRequest {
            identity: identity(),
            mode: mode(0x3231_564E, 1920, 1080, 60000, 1001),
        },
        audio: AudioSelection::Enabled {
            source: source_identity(),
        },
    }
}

fn preferences() -> LocalPreferences {
    LocalPreferences {
        gain: PlaybackGain::new(73, false).unwrap(),
        fullscreen: true,
    }
}

/// Mints an `AppliedSettings` through the only legitimate producer: a full
/// verified open on the product model.
fn applied_settings(settings: DraftSettings) -> AppliedSettings {
    let mut model = ProductModel::new(settings);
    model
        .apply(model.state_identity(), model.draft().revision)
        .unwrap();
    let request = model.validation_request().unwrap().clone();
    let Some(ModelEffect::Open { key, .. }) = model.validation_succeeded(&request) else {
        panic!("initial open effect missing");
    };
    model.open_verified(key);
    model.last_valid().unwrap().clone()
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
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("expected loaded document, got {outcome:?}");
    };
    assert!(!store.is_refused());
    assert_eq!(document.applied.as_ref(), Some(&draft_settings()));
    assert_eq!(document.preferences, preferences());
    assert_eq!(store.snapshot(), Some(&document));
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn required_nullable_fields_distinguish_null_from_missing() {
    let directory = temp_dir("nullable");
    let path = settings_path(&directory);

    // applied: null is explicit and valid.
    fs::write(&path, document_with_applied("null")).unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("applied:null must load, got {outcome:?}");
    };
    assert_eq!(document.applied, None);

    // serial: null is valid (absent serial), missing the key entirely is a
    // schema violation.
    fs::write(
        &path,
        document_with_applied(&APPLIED.replace(r#""serial":"SER42""#, r#""serial":null"#)),
    )
    .unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("serial:null must load, got {outcome:?}");
    };
    assert_eq!(document.applied.unwrap().video.identity.serial(), None);

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
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("retained:null must load, got {outcome:?}");
    };
    assert_eq!(
        document.applied.unwrap().audio,
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
                store.save_preferences(preferences()),
                Err(SettingsWriteError::RefusedOriginal { .. })
            ),
            "{label}: preference autosave must be blocked"
        );
        assert_eq!(fs::read(&path).unwrap(), json.as_bytes(), "{label}");
        assert!(
            matches!(
                store.save_applied(&applied, preferences()),
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
    for version in ["0", "2", "999", "18446744073709551615"] {
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
        store.save_preferences(preferences()),
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
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("expected loaded document");
    };
    assert_eq!(document.applied.as_ref(), Some(&draft_settings()));
    assert_eq!(document.preferences, preferences());

    let applied = applied_settings(draft_settings());
    let outcome = store.save_applied(&applied, preferences()).unwrap();
    assert!(outcome.is_confirmed());
    assert!(matches!(outcome.durability(), Durability::Confirmed));

    let (reloaded, reload_outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded(reloaded_document) = reload_outcome else {
        panic!("expected reloaded document");
    };
    assert_eq!(reloaded_document.applied.as_ref(), Some(&draft_settings()));
    assert_eq!(reloaded_document.preferences, preferences());
    assert_eq!(reloaded.snapshot(), Some(&reloaded_document));
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
        video: ModeRequest {
            identity: DeviceIdentity::new(
                0x1234,
                0x5678,
                UsbTopology::new("usb-wire0".to_owned(), vec![NonZeroU8::new(7).unwrap()]).unwrap(),
                None,
            )
            .unwrap(),
            mode: mode(0x3231_564E, 1280, 720, 120, 2),
        },
        audio: AudioSelection::Disabled { retained: None },
    };
    let applied = applied_settings(draft);
    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Missing));
    store
        .save_applied(&applied, LocalPreferences::default())
        .unwrap();

    let bytes = fs::read_to_string(&path).unwrap();
    assert!(
        bytes.contains(r#""fps":{"numerator":60,"denominator":1}"#),
        "120/2 must normalize to 60/1: {bytes}"
    );

    // 60000/1001 keeps its exact rational form on disk.
    let applied = applied_settings(draft_settings());
    store.save_applied(&applied, preferences()).unwrap();
    let bytes = fs::read_to_string(&path).unwrap();
    assert!(
        bytes.contains(r#""fps":{"numerator":60000,"denominator":1001}"#),
        "60000/1001 must be preserved exactly: {bytes}"
    );

    let (store, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("expected loaded document");
    };
    let stored = document.applied.as_ref().unwrap();
    assert_eq!(stored.video.mode.rate, FrameRate::new(60000, 1001).unwrap());
    assert_eq!(stored.video.mode.rate.numerator(), 60000);
    assert_eq!(stored.video.mode.rate.denominator(), 1001);
    assert_eq!(store.snapshot(), Some(&document));
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn preferences_only_save_preserves_published_applied() {
    let directory = temp_dir("prefs-preserve");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document()).unwrap();

    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("expected loaded document");
    };
    let applied_before = document.applied.clone();

    let new_preferences = LocalPreferences {
        gain: PlaybackGain::new(42, true).unwrap(),
        fullscreen: false,
    };
    store.save_preferences(new_preferences).unwrap();

    assert_eq!(store.snapshot().unwrap().applied, applied_before);
    assert_eq!(store.snapshot().unwrap().preferences, new_preferences);

    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("expected reloaded document");
    };
    assert_eq!(document.applied, applied_before);
    assert_eq!(document.preferences, new_preferences);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn preferences_only_save_on_missing_file_writes_null_applied() {
    let directory = temp_dir("prefs-missing");
    let path = settings_path(&directory);
    let (mut store, outcome) = SettingsStore::load(path.clone());
    assert!(matches!(outcome, LoadOutcome::Missing));

    store.save_preferences(preferences()).unwrap();
    let bytes = fs::read_to_string(&path).unwrap();
    assert!(bytes.contains(r#""applied":null"#), "bytes: {bytes}");
    assert_eq!(store.snapshot().unwrap().applied, None);
    assert_eq!(store.snapshot().unwrap().preferences, preferences());
    fs::remove_dir_all(&directory).unwrap();
}

// ---------------------------------------------------------------------------
// Refusal blocks automatic writes
// ---------------------------------------------------------------------------

#[test]
fn refusal_blocks_save_applied_and_save_preferences_byte_for_byte() {
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
        store.save_applied(&applied, preferences()),
        Err(SettingsWriteError::RefusedOriginal { .. })
    ));
    assert!(matches!(
        store.save_preferences(preferences()),
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
    let LoadOutcome::Loaded(previous) = outcome else {
        panic!("expected valid original");
    };
    let applied = applied_settings(draft_settings());
    let invalid = LocalPreferences {
        gain: PlaybackGain {
            volume_percent: 101,
            muted: true,
        },
        fullscreen: false,
    };
    for operation in ["applied", "preferences", "reset"] {
        let result = match operation {
            "applied" => store.save_applied(&applied, invalid),
            "preferences" => store.save_preferences(invalid),
            "reset" => store.reset(invalid),
            _ => unreachable!(),
        };
        assert!(
            matches!(result, Err(SettingsWriteError::InvalidValue { .. })),
            "{operation}"
        );
        assert_eq!(store.snapshot(), Some(&previous), "{operation}");
        assert_eq!(fs::read(&path).unwrap(), original.as_bytes(), "{operation}");
        assert_no_temp_files(&directory);
    }
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn failed_reset_preserves_refusal_original_and_blocks_automatic_saves() {
    let directory = temp_dir("failed-reset");
    let path = settings_path(&directory);
    let original = br#"{"schema_version":2,"future":"preserve this original"}"#;
    fs::write(&path, original).unwrap();
    let mut store = SettingsStore::with_injected_fault(path.clone(), WriteFaultPoint::PartialWrite);
    assert!(store.is_refused());

    let invalid = LocalPreferences {
        gain: PlaybackGain {
            volume_percent: 101,
            muted: true,
        },
        fullscreen: false,
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
        LoadOutcome::Refused(SettingsLoadError::UnsupportedVersion { version: 2, .. })
    ));
    let applied = applied_settings(draft_settings());
    assert!(matches!(
        store.save_applied(&applied, preferences()),
        Err(SettingsWriteError::RefusedOriginal { .. })
    ));
    assert!(matches!(
        store.save_preferences(preferences()),
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
        attempted.video.mode = mode(0x3231_564E, 1280, 720, 60, 1);
        attempted.audio = AudioSelection::Disabled { retained: None };
        let applied = applied_settings(attempted);
        let attempted_preferences = LocalPreferences {
            gain: PlaybackGain::new(42, true).unwrap(),
            fullscreen: false,
        };
        let result = store.save_applied(&applied, attempted_preferences);
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
        let LoadOutcome::Loaded(reloaded) = reloaded else {
            panic!("{point:?}: original must stay loadable immediately after failure");
        };
        assert_eq!(reloaded, previous, "{point:?}");

        // The fault fires once: the next write is a normal committed write.
        let outcome = store.save_preferences(attempted_preferences).unwrap();
        assert!(outcome.is_confirmed(), "{point:?}");
        let (_, outcome) = SettingsStore::load(path);
        let LoadOutcome::Loaded(document) = outcome else {
            panic!("{point:?}: next preference write must load");
        };
        assert_eq!(document.applied, previous.applied, "{point:?}");
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
    store.save_preferences(preferences()).unwrap();
    let complete = fs::read(&path).unwrap();

    let mut store = SettingsStore::with_injected_fault(path.clone(), WriteFaultPoint::PartialWrite);
    let previous = store.snapshot().unwrap().clone();
    let applied = applied_settings(draft_settings());
    let attempted_preferences = LocalPreferences {
        gain: PlaybackGain::new(42, true).unwrap(),
        fullscreen: false,
    };
    assert!(matches!(
        store.save_applied(&applied, attempted_preferences),
        Err(SettingsWriteError::WriteTemporary { .. })
    ));
    // The target and published snapshot retain the previous complete document.
    assert_eq!(fs::read(&path).unwrap(), complete);
    assert_eq!(store.snapshot(), Some(&previous));
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("partial write must leave the original immediately loadable");
    };
    assert_eq!(document, previous);
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
    assert_eq!(
        store.snapshot().map(|stored| stored.applied.clone()),
        Some(None)
    );
    assert_eq!(store.snapshot().unwrap().preferences, preferences());

    let (mut reloaded, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("reset must produce a loadable file");
    };
    assert_eq!(document.applied, None);
    assert_eq!(document.preferences, preferences());

    // After reset, a preference save keeps applied explicitly null.
    let later = LocalPreferences {
        gain: PlaybackGain::new(15, true).unwrap(),
        fullscreen: false,
    };
    reloaded.save_preferences(later).unwrap();
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("expected loaded document after later save");
    };
    assert_eq!(document.applied, None);
    assert_eq!(document.preferences, later);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn reset_refuses_nothing_about_the_previous_content_and_is_explicit_only() {
    let directory = temp_dir("reset-explicit");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document()).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(_) = outcome else {
        panic!("expected loaded document");
    };

    // save_preferences must NOT clear a published applied document.
    store.save_preferences(LocalPreferences::default()).unwrap();
    assert!(store.snapshot().unwrap().applied.is_some());

    // Only reset replaces the whole document.
    store.reset(LocalPreferences::default()).unwrap();
    assert_eq!(store.snapshot().unwrap().applied, None);
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
        let outcome = store.save_applied(&applied, preferences()).unwrap();
        let WriteOutcome::Committed { durability } = &outcome;
        assert!(!outcome.is_confirmed(), "{point:?}");
        assert!(
            matches!(durability, Durability::Unconfirmed(_)),
            "{point:?}"
        );

        // The file is complete and loadable, and the store published it.
        let (reloaded, outcome) = SettingsStore::load(path);
        let LoadOutcome::Loaded(document) = outcome else {
            panic!("{point:?}: committed file must load");
        };
        assert_eq!(document.applied.as_ref(), Some(&draft_settings()));
        assert_eq!(store.snapshot(), Some(&document));
        assert_eq!(document.preferences, preferences());
        assert_eq!(reloaded.snapshot(), Some(&document));
        assert!(reloaded.snapshot().unwrap().applied.is_some());
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
    store.save_preferences(preferences()).unwrap();
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
        matches!(outcome, LoadOutcome::Loaded(_)),
        "stray temp file must not affect loading: {outcome:?}"
    );
    assert!(!store.is_refused());
    fs::remove_dir_all(&directory).unwrap();
}
