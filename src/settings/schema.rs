//! Responsibility: Strict v1 DTO, validated domain conversion, all-or-nothing decode/encode.
//! Allowed dependencies: `domain`, serde serialization only.
//!
//! The DTO is deliberately private: only `decode` and `encode` cross the
//! boundary, and every value crossing back into the domain is rebuilt through
//! its validating constructor. There is no migration path: a document whose
//! `schema_version` is not 1 is refused outright.

use std::fmt;
use std::num::NonZeroU8;

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::store::{LocalPreferences, StoredDocument};
use crate::domain::capture::{
    AudioError, AudioSelection, AudioSourceIdentity, CaptureDataError, CaptureMode, CapturedFourCc,
    DeviceIdentity, FrameRate, FrameSize, ModeRequest, PlaybackGain, UsbTopology,
};
use crate::domain::state::DraftSettings;

/// On-disk schema version this module reads and writes.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// Value-level rejection after a structurally valid document was decoded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SettingsValidationError {
    #[error("video identity is invalid: {0}")]
    VideoIdentity(#[from] CaptureDataError),
    #[error("audio selection is invalid: {0}")]
    Audio(#[from] AudioError),
}

/// Internal decode failure taxonomy; `store` adds the file path.
#[derive(Debug, Error)]
pub(crate) enum SchemaError {
    #[error("settings file is not valid JSON: {0}")]
    MalformedJson(#[source] serde_json::Error),
    #[error("settings file violates schema v1: {0}")]
    Schema(#[source] serde_json::Error),
    #[error("settings file uses unsupported schema version {0}")]
    UnsupportedVersion(u64),
    #[error("settings file contains invalid values: {0}")]
    InvalidValue(#[from] SettingsValidationError),
}

/// Internal encode failure taxonomy; `store` shapes the public error.
#[derive(Debug, Error)]
pub(crate) enum EncodeError {
    #[error("settings document could not be serialized: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("settings document contains invalid values: {0}")]
    InvalidValue(#[from] SettingsValidationError),
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

/// Version discriminator for the second decode pass. It tolerates unknown
/// sibling keys on purpose: a future v2 document must surface as
/// `UnsupportedVersion`, never as a schema violation.
#[derive(Debug)]
struct VersionProbe {
    schema_version: u64,
}

impl<'de> Deserialize<'de> for VersionProbe {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Fields {
            schema_version: u64,
        }
        let fields: Fields = deserialize_object(deserializer)?;
        Ok(Self {
            schema_version: fields.schema_version,
        })
    }
}

/// Delegate named-field validation to Serde only after requiring a JSON object.
/// Derived struct deserializers alone also accept positional sequences.
fn deserialize_object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct ObjectVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a JSON object")
        }

        fn visit_map<A>(self, map: A) -> Result<T, A::Error>
        where
            A: MapAccess<'de>,
        {
            T::deserialize(de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_map(ObjectVisitor(std::marker::PhantomData))
}

// Keep each DTO's field list in one place while retaining Serde's strict
// unknown/duplicate/missing-field checks inside the object-only boundary.
macro_rules! object_dto {
    ($(#[$attribute:meta])* struct $name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        $(#[$attribute])*
        struct $name {
            $($field: $ty),*
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Fields {
                    $($field: $ty),*
                }
                let fields: Fields = deserialize_object(deserializer)?;
                Ok(Self { $($field: fields.$field),* })
            }
        }
    };
}

#[derive(Debug)]
struct DocumentV1 {
    applied: Option<AppliedV1>,
    preferences: PreferencesV1,
}

object_dto! {
    #[derive(Debug)]
    struct AppliedV1 {
        video: VideoV1,
        audio: AudioV1,
    }
}

object_dto! {
    #[derive(Debug)]
    struct VideoV1 {
        identity: DeviceIdentityV1,
        mode: CaptureModeV1,
    }
}

#[derive(Debug)]
struct DeviceIdentityV1 {
    vendor_id: u16,
    product_id: u16,
    topology: UsbTopologyV1,
    serial: Option<String>,
}

object_dto! {
    #[derive(Debug)]
    struct UsbTopologyV1 {
        controller: String,
        ports: Vec<NonZeroU8>,
    }
}

object_dto! {
    #[derive(Debug, Serialize)]
    struct CaptureModeV1 {
        captured_fourcc: u32,
        width: u32,
        height: u32,
        fps: FrameRateV1,
    }
}

object_dto! {
    #[derive(Debug, Serialize)]
    struct FrameRateV1 {
        numerator: u32,
        denominator: u32,
    }
}

#[derive(Debug)]
enum AudioV1 {
    Enabled { source: AudioSourceV1 },
    Disabled { retained: Option<AudioSourceV1> },
}

object_dto! {
    #[derive(Debug)]
    struct AudioSourceV1 {
        name: String,
        stable_properties: Vec<StablePropertyV1>,
    }
}

object_dto! {
    #[derive(Debug)]
    struct StablePropertyV1 {
        key: String,
        value: String,
    }
}

object_dto! {
    #[derive(Debug, Clone, Copy, Serialize)]
    struct PreferencesV1 {
        volume_percent: u8,
        muted: bool,
        fullscreen: bool,
    }
}

// ---------------------------------------------------------------------------
// Strict decoding
// ---------------------------------------------------------------------------

const DOCUMENT_FIELDS: &[&str] = &["schema_version", "applied", "preferences"];
const IDENTITY_FIELDS: &[&str] = &["vendor_id", "product_id", "topology", "serial"];
const AUDIO_FIELDS: &[&str] = &["kind", "source", "retained"];

impl<'de> Deserialize<'de> for DocumentV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_struct("DocumentV1", DOCUMENT_FIELDS, DocumentVisitor)
    }
}

struct DocumentVisitor;

impl<'de> Visitor<'de> for DocumentVisitor {
    type Value = DocumentV1;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("a settings document object with schema_version, applied and preferences")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut schema_version: Option<u64> = None;
        let mut applied: Option<Option<AppliedV1>> = None;
        let mut preferences: Option<PreferencesV1> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "schema_version" => {
                    if schema_version.is_some() {
                        return Err(de::Error::duplicate_field("schema_version"));
                    }
                    schema_version = Some(map.next_value()?);
                }
                "applied" => {
                    if applied.is_some() {
                        return Err(de::Error::duplicate_field("applied"));
                    }
                    // Required-nullable: an absent key is a schema violation,
                    // an explicit null decodes to `None`.
                    applied = Some(map.next_value()?);
                }
                "preferences" => {
                    if preferences.is_some() {
                        return Err(de::Error::duplicate_field("preferences"));
                    }
                    preferences = Some(map.next_value()?);
                }
                other => return Err(de::Error::unknown_field(other, DOCUMENT_FIELDS)),
            }
        }

        let schema_version =
            schema_version.ok_or_else(|| de::Error::missing_field("schema_version"))?;
        let applied = applied.ok_or_else(|| de::Error::missing_field("applied"))?;
        let preferences = preferences.ok_or_else(|| de::Error::missing_field("preferences"))?;

        // Unreachable through `decode` (the probe gates first) but keeps the
        // visitor self-contained if it is ever reused.
        if schema_version != SCHEMA_VERSION {
            return Err(de::Error::invalid_value(
                de::Unexpected::Unsigned(schema_version),
                &"schema_version 1",
            ));
        }

        Ok(DocumentV1 {
            applied,
            preferences,
        })
    }
}

impl<'de> Deserialize<'de> for DeviceIdentityV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_struct("DeviceIdentityV1", IDENTITY_FIELDS, DeviceIdentityVisitor)
    }
}

struct DeviceIdentityVisitor;

impl<'de> Visitor<'de> for DeviceIdentityVisitor {
    type Value = DeviceIdentityV1;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .write_str("a device identity object with vendor_id, product_id, topology and serial")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut vendor_id: Option<u16> = None;
        let mut product_id: Option<u16> = None;
        let mut topology: Option<UsbTopologyV1> = None;
        let mut serial: Option<Option<String>> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "vendor_id" => {
                    if vendor_id.is_some() {
                        return Err(de::Error::duplicate_field("vendor_id"));
                    }
                    vendor_id = Some(map.next_value()?);
                }
                "product_id" => {
                    if product_id.is_some() {
                        return Err(de::Error::duplicate_field("product_id"));
                    }
                    product_id = Some(map.next_value()?);
                }
                "topology" => {
                    if topology.is_some() {
                        return Err(de::Error::duplicate_field("topology"));
                    }
                    topology = Some(map.next_value()?);
                }
                "serial" => {
                    if serial.is_some() {
                        return Err(de::Error::duplicate_field("serial"));
                    }
                    // Required-nullable: missing ≠ null.
                    serial = Some(map.next_value()?);
                }
                other => return Err(de::Error::unknown_field(other, IDENTITY_FIELDS)),
            }
        }

        Ok(DeviceIdentityV1 {
            vendor_id: vendor_id.ok_or_else(|| de::Error::missing_field("vendor_id"))?,
            product_id: product_id.ok_or_else(|| de::Error::missing_field("product_id"))?,
            topology: topology.ok_or_else(|| de::Error::missing_field("topology"))?,
            serial: serial.ok_or_else(|| de::Error::missing_field("serial"))?,
        })
    }
}

impl<'de> Deserialize<'de> for AudioV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // `deserialize_map`, not `deserialize_struct`: the tagged union must
        // never accept the sequence form, and the tag decides which sibling
        // fields are legal. The visitor's `expecting` names the shape.
        deserializer.deserialize_map(AudioVisitor)
    }
}

struct AudioVisitor;

impl<'de> Visitor<'de> for AudioVisitor {
    type Value = AudioV1;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an audio selection object with a kind tag of enabled or disabled")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut kind: Option<String> = None;
        let mut source: Option<Option<AudioSourceV1>> = None;
        let mut retained: Option<Option<AudioSourceV1>> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "kind" => {
                    if kind.is_some() {
                        return Err(de::Error::duplicate_field("kind"));
                    }
                    kind = Some(map.next_value()?);
                }
                "source" => {
                    if source.is_some() {
                        return Err(de::Error::duplicate_field("source"));
                    }
                    source = Some(map.next_value()?);
                }
                "retained" => {
                    if retained.is_some() {
                        return Err(de::Error::duplicate_field("retained"));
                    }
                    retained = Some(map.next_value()?);
                }
                other => return Err(de::Error::unknown_field(other, AUDIO_FIELDS)),
            }
        }

        let kind = kind.ok_or_else(|| de::Error::missing_field("kind"))?;
        match kind.as_str() {
            "enabled" => {
                if retained.is_some() {
                    return Err(de::Error::unknown_field("retained", &["source"]));
                }
                let source = source
                    .ok_or_else(|| de::Error::missing_field("source"))?
                    .ok_or_else(|| {
                        de::Error::invalid_type(de::Unexpected::Unit, &"an enabled audio source")
                    })?;
                Ok(AudioV1::Enabled { source })
            }
            "disabled" => {
                if source.is_some() {
                    return Err(de::Error::unknown_field("source", &["retained"]));
                }
                // Required even when null: a disabled selection always states
                // whether a source was retained.
                let retained = retained.ok_or_else(|| de::Error::missing_field("retained"))?;
                Ok(AudioV1::Disabled { retained })
            }
            other => Err(de::Error::unknown_variant(other, &["enabled", "disabled"])),
        }
    }
}

// ---------------------------------------------------------------------------
// Decode pipeline
// ---------------------------------------------------------------------------

/// Three strictly separated passes over the same bytes:
/// 1. syntax only (JSON well-formedness);
/// 2. version discriminator, tolerant of unknown keys;
/// 3. strict v1 shape, then validated domain conversion.
pub(crate) fn decode(bytes: &[u8]) -> Result<StoredDocument, SchemaError> {
    serde_json::from_slice::<IgnoredAny>(bytes).map_err(SchemaError::MalformedJson)?;

    let probe = serde_json::from_slice::<VersionProbe>(bytes).map_err(SchemaError::Schema)?;
    if probe.schema_version != SCHEMA_VERSION {
        return Err(SchemaError::UnsupportedVersion(probe.schema_version));
    }

    let document = serde_json::from_slice::<DocumentV1>(bytes).map_err(SchemaError::Schema)?;
    stored_from_document(document).map_err(SchemaError::InvalidValue)
}

fn stored_from_document(document: DocumentV1) -> Result<StoredDocument, SettingsValidationError> {
    Ok(StoredDocument {
        applied: document.applied.map(applied_from_dto).transpose()?,
        preferences: preferences_from_dto(document.preferences)?,
    })
}

// ---------------------------------------------------------------------------
// Encode pipeline
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DocumentWrite<'a> {
    schema_version: u64,
    applied: Option<AppliedWrite<'a>>,
    preferences: PreferencesV1,
}

#[derive(Serialize)]
struct AppliedWrite<'a> {
    video: VideoWrite<'a>,
    audio: AudioWrite<'a>,
}

#[derive(Serialize)]
struct VideoWrite<'a> {
    identity: DeviceIdentityWrite<'a>,
    mode: CaptureModeV1,
}

#[derive(Serialize)]
struct DeviceIdentityWrite<'a> {
    vendor_id: u16,
    product_id: u16,
    topology: UsbTopologyWrite<'a>,
    serial: Option<&'a str>,
}

#[derive(Serialize)]
struct UsbTopologyWrite<'a> {
    controller: &'a str,
    ports: &'a [NonZeroU8],
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum AudioWrite<'a> {
    Enabled {
        source: AudioSourceWrite<'a>,
    },
    Disabled {
        retained: Option<AudioSourceWrite<'a>>,
    },
}

#[derive(Serialize)]
struct AudioSourceWrite<'a> {
    name: &'a str,
    #[serde(serialize_with = "serialize_stable_properties")]
    stable_properties: &'a [(String, String)],
}

fn serialize_stable_properties<S>(
    properties: &[(String, String)],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    #[derive(Serialize)]
    struct Property<'a> {
        key: &'a str,
        value: &'a str,
    }

    let mut sequence = serializer.serialize_seq(Some(properties.len()))?;
    for (key, value) in properties {
        sequence.serialize_element(&Property { key, value })?;
    }
    sequence.end()
}

/// Validate public mutable preferences before serialization. Identity, topology,
/// source properties, size and rational rate are private constructor-validated
/// domain values: their borrowed wire views preserve those invariants without
/// rebuilding strings/vectors or sorting already-normalized properties.
pub(crate) fn encode(document: &StoredDocument) -> Result<Vec<u8>, EncodeError> {
    let preferences = preferences_to_dto(document.preferences)?;
    let document = DocumentWrite {
        schema_version: SCHEMA_VERSION,
        applied: document.applied.as_ref().map(applied_to_dto),
        preferences,
    };
    let mut bytes = serde_json::to_vec(&document).map_err(EncodeError::Serialize)?;
    bytes.push(b'\n');
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Domain conversions
// ---------------------------------------------------------------------------

fn applied_to_dto(applied: &DraftSettings) -> AppliedWrite<'_> {
    AppliedWrite {
        video: video_to_dto(&applied.video),
        audio: audio_to_dto(&applied.audio),
    }
}

fn applied_from_dto(applied: AppliedV1) -> Result<DraftSettings, SettingsValidationError> {
    Ok(DraftSettings {
        video: video_from_dto(applied.video)?,
        audio: audio_from_dto(applied.audio)?,
    })
}

fn video_to_dto(video: &ModeRequest) -> VideoWrite<'_> {
    let identity = &video.identity;
    let topology = identity.topology();
    let mode = video.mode;
    VideoWrite {
        identity: DeviceIdentityWrite {
            vendor_id: identity.vendor_id(),
            product_id: identity.product_id(),
            topology: UsbTopologyWrite {
                controller: topology.controller(),
                ports: topology.ports(),
            },
            serial: identity.serial(),
        },
        mode: CaptureModeV1 {
            captured_fourcc: mode.captured_fourcc.kernel_value(),
            width: mode.size.width(),
            height: mode.size.height(),
            fps: FrameRateV1 {
                numerator: mode.rate.numerator(),
                denominator: mode.rate.denominator(),
            },
        },
    }
}

fn video_from_dto(video: VideoV1) -> Result<ModeRequest, SettingsValidationError> {
    let DeviceIdentityV1 {
        vendor_id,
        product_id,
        topology,
        serial,
    } = video.identity;
    let topology = UsbTopology::new(topology.controller, topology.ports)?;
    let identity = DeviceIdentity::new(vendor_id, product_id, topology, serial)?;
    let mode = video.mode;

    Ok(ModeRequest {
        identity,
        mode: CaptureMode {
            captured_fourcc: CapturedFourCc::from_kernel(mode.captured_fourcc),
            size: FrameSize::new(mode.width, mode.height)?,
            rate: FrameRate::new(mode.fps.numerator, mode.fps.denominator)?,
        },
    })
}

fn audio_to_dto(selection: &AudioSelection) -> AudioWrite<'_> {
    match selection {
        AudioSelection::Enabled { source } => AudioWrite::Enabled {
            source: audio_source_to_dto(source),
        },
        AudioSelection::Disabled { retained } => AudioWrite::Disabled {
            retained: retained.as_ref().map(audio_source_to_dto),
        },
    }
}

fn audio_from_dto(selection: AudioV1) -> Result<AudioSelection, SettingsValidationError> {
    Ok(match selection {
        AudioV1::Enabled { source } => AudioSelection::Enabled {
            source: audio_source_from_dto(source)?,
        },
        AudioV1::Disabled { retained } => AudioSelection::Disabled {
            retained: retained.map(audio_source_from_dto).transpose()?,
        },
    })
}

fn audio_source_to_dto(source: &AudioSourceIdentity) -> AudioSourceWrite<'_> {
    AudioSourceWrite {
        name: source.name(),
        stable_properties: source.stable_properties(),
    }
}

fn audio_source_from_dto(
    source: AudioSourceV1,
) -> Result<AudioSourceIdentity, SettingsValidationError> {
    let properties = source
        .stable_properties
        .into_iter()
        .map(|property| (property.key, property.value))
        .collect();
    Ok(AudioSourceIdentity::new(source.name, properties)?)
}

fn preferences_to_dto(
    preferences: LocalPreferences,
) -> Result<PreferencesV1, SettingsValidationError> {
    let gain = PlaybackGain::new(preferences.gain.volume_percent, preferences.gain.muted)?;
    Ok(PreferencesV1 {
        volume_percent: gain.volume_percent,
        muted: gain.muted,
        fullscreen: preferences.fullscreen,
    })
}

fn preferences_from_dto(
    preferences: PreferencesV1,
) -> Result<LocalPreferences, SettingsValidationError> {
    Ok(LocalPreferences {
        gain: PlaybackGain::new(preferences.volume_percent, preferences.muted)?,
        fullscreen: preferences.fullscreen,
    })
}
