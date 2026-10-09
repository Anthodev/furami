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
use furami::domain::filters::{
    ColorLevels, EqParams, EqValues, Filter, FilterChain, FilterEntry, FormatParams, Hqdn3dParams,
    Hqdn3dValues, SdrGamma, SdrMatrix,
};
use furami::domain::output::{PersistentOutputChoice, SinkIdentity};
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
        filters: furami::domain::filters::FilterChain::default(),
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
        output: PersistentOutputChoice::Auto,
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
        filters: furami::domain::filters::FilterChain::default(),
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
        output: PersistentOutputChoice::Auto,
    };
    store.save_preferences(new_preferences.clone()).unwrap();

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
    store.save_preferences(manual.clone()).unwrap();

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
    let LoadOutcome::Loaded(document) = outcome else {
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

    store.save_preferences(preferences()).unwrap();
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
    let LoadOutcome::Loaded(document) = outcome else {
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
        store.save_preferences(manual_output_preferences()),
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
    let LoadOutcome::Loaded(previous) = outcome else {
        panic!("expected valid original");
    };
    let applied_before = previous.applied.clone();

    // Only the output choice changes; gain/fullscreen and the published
    // applied document are preserved through a preferences-only save.
    let mut switched = previous.preferences.clone();
    switched.output = PersistentOutputChoice::Manual(manual_sink_identity());
    store.save_preferences(switched.clone()).unwrap();
    assert_eq!(store.snapshot().unwrap().applied, applied_before);
    assert_eq!(store.snapshot().unwrap().preferences, switched);

    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("manual output document must reload");
    };
    assert_eq!(document.applied, applied_before);
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
        output: PersistentOutputChoice::Auto,
    };
    for operation in ["applied", "preferences", "reset"] {
        let result = match operation {
            "applied" => store.save_applied(&applied, invalid.clone()),
            "preferences" => store.save_preferences(invalid.clone()),
            "reset" => store.reset(invalid.clone()),
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
        output: PersistentOutputChoice::Auto,
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
            output: PersistentOutputChoice::Auto,
        };
        let result = store.save_applied(&applied, attempted_preferences.clone());
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
        let outcome = store
            .save_preferences(attempted_preferences.clone())
            .unwrap();
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
        output: PersistentOutputChoice::Auto,
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
        output: PersistentOutputChoice::Auto,
    };
    reloaded.save_preferences(later.clone()).unwrap();
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

// ---------------------------------------------------------------------------
// Applied filter chains (FUR-015)
// ---------------------------------------------------------------------------

const THREE_FILTER_ENTRIES: &str = concat!(
    r#"{"entries":["#,
    r#"{"label":"Levels","enabled":true,"filter":{"kind":"format","parameters":{"matrix":"bt709","levels":"limited","gamma":"bt1886"}}},"#,
    r#"{"label":"Contrast","enabled":false,"filter":{"kind":"eq","parameters":{"contrast":1.5,"brightness":0.1,"saturation":1.2,"gamma":1.1,"gamma_r":1.0,"gamma_g":1.0,"gamma_b":1.0,"gamma_weight":0.5}}},"#,
    r#"{"label":"Denoise","enabled":true,"filter":{"kind":"hqdn3d","parameters":{"luma_spatial":4.0,"chroma_spatial":3.0,"luma_tmp":6.0,"chroma_tmp":4.5}}}"#,
    r#"]}"#,
);

/// Inject a `filters` member into the object-form `APPLIED` fixture.
fn applied_with_filters(filters_json: &str) -> String {
    let inner = APPLIED
        .strip_suffix('}')
        .expect("APPLIED fixture is an object");
    format!(r#"{inner},"filters":{filters_json}}}"#)
}

fn three_entry_chain() -> FilterChain {
    FilterChain::new(vec![
        FilterEntry::new(
            "Levels".to_owned(),
            Filter::Format(FormatParams::new(
                SdrMatrix::Bt709,
                ColorLevels::Limited,
                SdrGamma::Bt1886,
            )),
            true,
        ),
        FilterEntry::new(
            "Contrast".to_owned(),
            Filter::Eq(
                EqParams::new(EqValues {
                    contrast: 1.5,
                    brightness: 0.1,
                    saturation: 1.2,
                    gamma: 1.1,
                    gamma_r: 1.0,
                    gamma_g: 1.0,
                    gamma_b: 1.0,
                    gamma_weight: 0.5,
                })
                .unwrap(),
            ),
            false,
        ),
        FilterEntry::new(
            "Denoise".to_owned(),
            Filter::Hqdn3d(
                Hqdn3dParams::new(Hqdn3dValues {
                    luma_spatial: 4.0,
                    chroma_spatial: 3.0,
                    luma_tmp: 6.0,
                    chroma_tmp: 4.5,
                })
                .unwrap(),
            ),
            true,
        ),
    ])
    .unwrap()
}

fn draft_with_filters(filters: FilterChain) -> DraftSettings {
    DraftSettings {
        filters,
        ..draft_settings()
    }
}

#[test]
fn pre_filter_v1_document_loads_empty_chain_and_reencodes_byte_for_byte() {
    let directory = temp_dir("filters-parity");
    let path = settings_path(&directory);
    let original = canonical_document();
    fs::write(&path, &original).unwrap();

    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("pre-filter v1 document must load, got {outcome:?}");
    };
    let loaded = document.applied.as_ref().expect("applied present");
    assert!(loaded.filters.entries().is_empty());
    assert_eq!(loaded, &draft_settings());

    store
        .save_applied(&applied_settings(draft_settings()), preferences())
        .unwrap();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{original}\n"),
        "an empty chain must not add a filters key"
    );
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn full_filter_chain_round_trips_exactly_through_the_store() {
    let directory = temp_dir("filters-roundtrip");
    let path = settings_path(&directory);
    fs::write(
        &path,
        document_with_applied(&applied_with_filters(THREE_FILTER_ENTRIES)),
    )
    .unwrap();

    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("three-entry chain must load, got {outcome:?}");
    };
    assert_eq!(
        document.applied.as_ref().unwrap().filters,
        three_entry_chain()
    );

    let applied = applied_settings(draft_with_filters(three_entry_chain()));
    store.save_applied(&applied, preferences()).unwrap();

    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(reloaded) = outcome else {
        panic!("re-encoded document must load");
    };
    let entries = reloaded.applied.as_ref().unwrap().filters.entries();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].label(), "Levels");
    assert!(entries[0].enabled());
    assert_eq!(entries[1].label(), "Contrast");
    assert!(!entries[1].enabled());
    assert_eq!(entries[2].label(), "Denoise");
    assert!(entries[2].enabled());
    assert_eq!(
        reloaded.applied.as_ref().unwrap().filters,
        three_entry_chain()
    );
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains(r#""filters":{"entries":["#),
        "the nonempty chain must be written"
    );
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn inert_labels_unicode_whitespace_controls_and_nul_survive_the_store() {
    let directory = temp_dir("filters-inert-labels");
    let path = settings_path(&directory);
    // NBSP as raw UTF-8, plus a JSON \n and a JSON \u0000: all are inert domain
    // text and must never be treated as a control surface.
    let filters = concat!(
        r#"{"entries":["#,
        r#"{"label":"\u00a0NBSP","enabled":true,"filter":{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}},"#,
        r#"{"label":"line\nbreak","enabled":true,"filter":{"kind":"eq","parameters":{"contrast":1.0,"brightness":0.0,"saturation":1.0,"gamma":1.0,"gamma_r":1.0,"gamma_g":1.0,"gamma_b":1.0,"gamma_weight":1.0}}},"#,
        r#"{"label":"nul\u0000byte","enabled":false,"filter":{"kind":"hqdn3d","parameters":{"luma_spatial":1.0,"chroma_spatial":1.0,"luma_tmp":1.0,"chroma_tmp":1.0}}}"#,
        r#"]}"#,
    );
    let expected = FilterChain::new(vec![
        FilterEntry::new(
            "\u{a0}NBSP".to_owned(),
            Filter::Format(FormatParams::new(
                SdrMatrix::Auto,
                ColorLevels::Auto,
                SdrGamma::Auto,
            )),
            true,
        ),
        FilterEntry::new(
            "line\nbreak".to_owned(),
            Filter::Eq(
                EqParams::new(EqValues {
                    contrast: 1.0,
                    brightness: 0.0,
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
        ),
        FilterEntry::new(
            "nul\0byte".to_owned(),
            Filter::Hqdn3d(
                Hqdn3dParams::new(Hqdn3dValues {
                    luma_spatial: 1.0,
                    chroma_spatial: 1.0,
                    luma_tmp: 1.0,
                    chroma_tmp: 1.0,
                })
                .unwrap(),
            ),
            false,
        ),
    ])
    .unwrap();

    fs::write(&path, document_with_applied(&applied_with_filters(filters))).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("inert labels must load, got {outcome:?}");
    };
    assert_eq!(document.applied.as_ref().unwrap().filters, expected);

    let applied = applied_settings(draft_with_filters(expected.clone()));
    store.save_applied(&applied, preferences()).unwrap();
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(reloaded) = outcome else {
        panic!("re-encoded inert labels must load");
    };
    assert_eq!(reloaded.applied.as_ref().unwrap().filters, expected);

    // The bytes on disk prove real JSON escaping rather than a stored echo.
    let written = fs::read_to_string(&path).unwrap();
    assert!(
        written.contains("line\\nbreak"),
        "newline must be escaped: {written}"
    );
    assert!(
        written.contains("nul\\u0000byte"),
        "NUL must be escaped: {written}"
    );
    assert!(
        written.contains('\u{a0}'),
        "NBSP must survive as raw UTF-8: {written}"
    );
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn all_disabled_nonempty_chain_is_serialized_and_reloaded_exactly() {
    let directory = temp_dir("filters-all-disabled");
    let path = settings_path(&directory);
    let filters = concat!(
        r#"{"entries":["#,
        r#"{"label":"a","enabled":false,"filter":{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}},"#,
        r#"{"label":"b","enabled":false,"filter":{"kind":"hqdn3d","parameters":{"luma_spatial":0.0,"chroma_spatial":0.0,"luma_tmp":0.0,"chroma_tmp":0.0}}}"#,
        r#"]}"#,
    );
    let expected = FilterChain::new(vec![
        FilterEntry::new(
            "a".to_owned(),
            Filter::Format(FormatParams::new(
                SdrMatrix::Auto,
                ColorLevels::Auto,
                SdrGamma::Auto,
            )),
            false,
        ),
        FilterEntry::new(
            "b".to_owned(),
            Filter::Hqdn3d(
                Hqdn3dParams::new(Hqdn3dValues {
                    luma_spatial: 0.0,
                    chroma_spatial: 0.0,
                    luma_tmp: 0.0,
                    chroma_tmp: 0.0,
                })
                .unwrap(),
            ),
            false,
        ),
    ])
    .unwrap();

    fs::write(&path, document_with_applied(&applied_with_filters(filters))).unwrap();
    let (mut store, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(document) = outcome else {
        panic!("all-disabled chain must load, got {outcome:?}");
    };
    assert_eq!(document.applied.as_ref().unwrap().filters, expected);

    let applied = applied_settings(draft_with_filters(expected.clone()));
    store.save_applied(&applied, preferences()).unwrap();
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains(r#""filters":{"entries":["#),
        "a nonempty all-disabled chain must still be written"
    );
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded(reloaded) = outcome else {
        panic!("all-disabled chain must reload");
    };
    assert_eq!(reloaded.applied.as_ref().unwrap().filters, expected);
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn malformed_filter_json_refuses_the_complete_original_and_blocks_saves_until_reset() {
    let valid_entry = r#"{"label":"a","enabled":true,"filter":{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}}"#;
    let duplicate_filters_key = {
        let inner = APPLIED
            .strip_suffix('}')
            .expect("APPLIED fixture is an object");
        let chain = r#"{"entries":[]}"#;
        let applied = format!(r#"{inner},"filters":{chain},"filters":{chain}}}"#);
        format!(r#"{{"schema_version":1,"applied":{applied},"preferences":{PREFS}}}"#)
    };
    let cases: Vec<(&str, String)> = vec![
        (
            "filters null",
            document_with_applied(&applied_with_filters("null")),
        ),
        (
            "filters array",
            document_with_applied(&applied_with_filters("[]")),
        ),
        (
            "filters string",
            document_with_applied(&applied_with_filters("\"entries\"")),
        ),
        (
            "filters unknown field",
            document_with_applied(&applied_with_filters(r#"{"entries":[],"command":"vf"}"#)),
        ),
        (
            "filters missing entries",
            document_with_applied(&applied_with_filters("{}")),
        ),
        (
            "entry positional array",
            document_with_applied(&applied_with_filters(&format!(
                r#"{{"entries":[[{valid_entry}]]}}"#
            ))),
        ),
        (
            "duplicate label",
            document_with_applied(&applied_with_filters(&format!(
                r#"{{"entries":[{valid_entry},{valid_entry}]}}"#
            ))),
        ),
        (
            "duplicate entry field",
            document_with_applied(&applied_with_filters(
                r#"{"entries":[{"label":"a","label":"b","enabled":true,"filter":{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}}]}"#,
            )),
        ),
        (
            "unknown entry field",
            document_with_applied(&applied_with_filters(
                r#"{"entries":[{"label":"a","enabled":true,"extra":1,"filter":{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}}]}"#,
            )),
        ),
        (
            "missing enabled",
            document_with_applied(&applied_with_filters(
                r#"{"entries":[{"label":"a","filter":{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}}]}"#,
            )),
        ),
        (
            "unknown filter kind",
            document_with_applied(&applied_with_filters(
                r#"{"entries":[{"label":"a","enabled":true,"filter":{"kind":"sharpen","parameters":{}}}]}"#,
            )),
        ),
        (
            "duplicate parameter key",
            document_with_applied(&applied_with_filters(
                r#"{"entries":[{"label":"a","enabled":true,"filter":{"kind":"format","parameters":{"matrix":"auto","matrix":"auto","levels":"auto","gamma":"auto"}}}]}"#,
            )),
        ),
        (
            "duplicate parameter key parameters-first",
            document_with_applied(&applied_with_filters(
                r#"{"entries":[{"label":"a","enabled":true,"filter":{"parameters":{"matrix":"auto","matrix":"auto","levels":"auto","gamma":"auto"},"kind":"format"}}]}"#,
            )),
        ),
        (
            "duplicate kind tag",
            document_with_applied(&applied_with_filters(
                r#"{"entries":[{"label":"a","enabled":true,"filter":{"kind":"format","kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}}]}"#,
            )),
        ),
        (
            "invalid params even disabled",
            document_with_applied(&applied_with_filters(
                r#"{"entries":[{"label":"a","enabled":false,"filter":{"kind":"eq","parameters":{"contrast":5000.0,"brightness":0.0,"saturation":1.0,"gamma":1.0,"gamma_r":1.0,"gamma_g":1.0,"gamma_b":1.0,"gamma_weight":1.0}}}]}"#,
            )),
        ),
        ("duplicate filters key", duplicate_filters_key),
    ];

    let directory = temp_dir("filters-violations");
    let path = settings_path(&directory);
    let applied = applied_settings(draft_with_filters(three_entry_chain()));
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
                store.save_applied(&applied, preferences()),
                Err(SettingsWriteError::RefusedOriginal { .. })
            ),
            "{label}: applied write must stay blocked"
        );
        assert!(
            matches!(
                store.save_preferences(preferences()),
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
            matches!(outcome, LoadOutcome::Loaded(_)),
            "{label}: reset must produce a loadable document"
        );
    }
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn full_chain_rename_fault_preserves_the_original_and_reports_persistence_separately() {
    let directory = temp_dir("filters-fault");
    let path = settings_path(&directory);
    fs::write(&path, canonical_document()).unwrap();
    let original = fs::read(&path).unwrap();

    let mut store = SettingsStore::with_injected_fault(path.clone(), WriteFaultPoint::BeforeRename);
    let previous = store.snapshot().unwrap().clone();
    let applied = applied_settings(draft_with_filters(three_entry_chain()));

    let result = store.save_applied(&applied, preferences());
    assert!(
        matches!(result, Err(SettingsWriteError::Rename { .. })),
        "persistence failure must be reported separately: {result:?}"
    );
    assert_eq!(store.snapshot(), Some(&previous));
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "original bytes must survive a rename fault"
    );
    assert_no_temp_files(&directory);
    let (_, outcome) = SettingsStore::load(path.clone());
    let LoadOutcome::Loaded(reloaded) = outcome else {
        panic!("original must stay loadable immediately after the fault");
    };
    assert_eq!(reloaded, previous);
    assert!(
        reloaded
            .applied
            .as_ref()
            .unwrap()
            .filters
            .entries()
            .is_empty()
    );

    // The fault fires once: the next write commits the full chain.
    let outcome = store.save_applied(&applied, preferences()).unwrap();
    assert!(outcome.is_confirmed());
    let (_, outcome) = SettingsStore::load(path);
    let LoadOutcome::Loaded(stored) = outcome else {
        panic!("committed chain must load");
    };
    assert_eq!(
        stored.applied.as_ref().unwrap().filters,
        three_entry_chain()
    );
    fs::remove_dir_all(&directory).unwrap();
}
