//! Responsibility: Strict versioned DTOs, validated conversion, atomic document codecs.
//! Allowed dependencies: domain and settings value types, serde serialization only.
//!
//! Version 1 migrates in memory through the same validated settings payload;
//! version 2 is the only write format. Neither decoder accepts partial recovery.

use std::fmt;
use std::num::NonZeroU8;

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::profiles::{ProfileBinding, ProfileValidationError, SourceProfile, SourceProfiles};
use super::store::{LocalPreferences, StoredDocument};
use crate::domain::capture::{
    AudioError, AudioSelection, AudioSourceIdentity, CaptureDataError, CaptureModeRequest,
    CapturedFourCc, DeviceIdentity, FrameRate, FrameSize, InputFormatPolicy, PlaybackGain,
    UsbTopology, VideoRequest,
};
use crate::domain::filters::FilterChain;
use crate::domain::output::{PersistentOutputChoice, SinkIdentity};
use crate::domain::render::RendererSettings;
use crate::domain::state::DraftSettings;

/// On-disk schema version written by this module; v1 is also read.
pub(crate) const SCHEMA_VERSION: u64 = 2;

/// Value-level rejection of a structurally valid document, on decode or on
/// encode before any bytes are written.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SettingsValidationError {
    #[error("video identity is invalid: {0}")]
    VideoIdentity(#[from] CaptureDataError),
    #[error("audio selection is invalid: {0}")]
    Audio(#[from] AudioError),
    /// Encode-only: an applied video request has no proven concrete format, so
    /// it must never be written as if a mode had been opened. The decoder always
    /// produces a concrete opening, so this can never arise on load.
    #[error("applied video request has no resolved capture format")]
    UnresolvedVideo,
    /// A manual pin and the resolved concrete format disagree, or an automatic
    /// policy resolves to a non-concrete FourCC: the document would not describe
    /// the verified opening. Guards both the encode and decode boundaries.
    #[error("applied video format policy does not match its resolved capture format")]
    VideoFormatPolicyMismatch,
    #[error("source profile collection is invalid: {0}")]
    Profiles(#[from] ProfileValidationError),
}

/// Internal decode failure taxonomy; `store` adds the file path.
#[derive(Debug, Error)]
pub(crate) enum SchemaError {
    #[error("settings file is not valid JSON: {0}")]
    MalformedJson(#[source] serde_json::Error),
    #[error("settings file violates its versioned schema: {0}")]
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

/// Version discriminator for the strict decode pass. Unknown sibling keys are
/// tolerated only here, preserving duplicate-version checks without a Value map.
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

#[derive(Debug)]
struct DocumentV2 {
    profiles: Vec<ProfileV2>,
    last_source: Option<DeviceIdentityV1>,
    preferences: PreferencesV1,
}

object_dto! {
    #[derive(Debug)]
    struct ProfileV2 {
        binding: BindingV2,
        settings: AppliedV1,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum BindingV2 {
    Serial,
    Topology,
}

/// The applied section. `video` and `audio` stay mandatory; `filters` and
/// `renderer` are absent-by-default for backward compatibility with documents
/// written before those settings existed. A present `filters`/`renderer` is
/// decoded straight into the validated domain value (never through an
/// intermediate `Value`), so an explicit `null` is a schema violation rather
/// than the default. The custom deserializer below replaces the `object_dto!`
/// derive for exactly that reason.
#[derive(Debug)]
struct AppliedV1 {
    video: VideoV1,
    audio: AudioV1,
    filters: FilterChain,
    renderer: RendererSettings,
}

/// The applied video request. `identity` and `mode` stay mandatory; the optional
/// `format_policy` marker is absent-by-default for backward compatibility with
/// documents written before automatic selection existed. A present marker is
/// decoded straight into the validated policy (never through an intermediate
/// `Value`), so an explicit `null` is a schema violation rather than the
/// manual default. The custom deserializer below replaces the `object_dto!`
/// derive for exactly that reason.
#[derive(Debug)]
struct VideoV1 {
    identity: DeviceIdentityV1,
    mode: CaptureModeV1,
    format_policy: Option<FormatPolicyV1>,
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

// Wire shape of a capture mode: the concrete proven opening. The automatic
// selection policy lives beside it as `VideoV1.format_policy`, not here.
object_dto! {
    #[derive(Debug, Serialize)]
    struct CaptureModeV1 {
        captured_fourcc: u32,
        width: u32,
        height: u32,
        fps: FrameRateV1,
    }
}

/// The only accepted `format_policy` marker. Manual is represented by omitting
/// the key entirely so pre-policy v1 documents keep their exact shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum FormatPolicyV1 {
    Auto,
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

#[derive(Debug)]
struct PreferencesV1 {
    volume_percent: u8,
    muted: bool,
    fullscreen: bool,
    /// Optional for backward compatibility: a strict v1 document written
    /// before the output choice existed has no `output` key and means `Auto`.
    /// An explicit `null` is a schema violation, not `Auto`.
    output: Option<OutputV1>,
    stats_hidden: bool,
}

#[derive(Debug)]
enum OutputV1 {
    Auto,
    Manual { sink: SinkV1 },
}

object_dto! {
    #[derive(Debug)]
    struct SinkV1 {
        name: String,
        stable_properties: Vec<StablePropertyV1>,
    }
}

// ---------------------------------------------------------------------------
// Strict decoding
// ---------------------------------------------------------------------------

const DOCUMENT_V1_FIELDS: &[&str] = &["schema_version", "applied", "preferences"];
const DOCUMENT_V2_FIELDS: &[&str] = &["schema_version", "profiles", "last_source", "preferences"];
const APPLIED_FIELDS: &[&str] = &["video", "audio", "filters", "renderer"];
const IDENTITY_FIELDS: &[&str] = &["vendor_id", "product_id", "topology", "serial"];
const VIDEO_FIELDS: &[&str] = &["identity", "mode", "format_policy"];
const AUDIO_FIELDS: &[&str] = &["kind", "source", "retained"];
const PREFS_FIELDS: &[&str] = &[
    "volume_percent",
    "muted",
    "fullscreen",
    "output",
    "stats_hidden",
];
const OUTPUT_FIELDS: &[&str] = &["kind", "sink"];

impl<'de> Deserialize<'de> for AppliedV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Not the `object_dto!` derive: `filters` is absent-by-default for
        // backward compatibility, which a fully strict field set cannot
        // express. A present `filters` decodes straight into the validated
        // `FilterChain` (never through `serde_json::Value`, which would
        // collapse duplicate object keys); an explicit `null` is refused.
        deserializer.deserialize_struct("AppliedV1", APPLIED_FIELDS, AppliedVisitor)
    }
}

struct AppliedVisitor;

impl<'de> Visitor<'de> for AppliedVisitor {
    type Value = AppliedV1;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "an applied settings object with video, audio and optional filters and renderer",
        )
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut video: Option<VideoV1> = None;
        let mut audio: Option<AudioV1> = None;
        let mut filters: Option<FilterChain> = None;
        let mut renderer: Option<RendererSettings> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "video" => {
                    if video.is_some() {
                        return Err(de::Error::duplicate_field("video"));
                    }
                    video = Some(map.next_value()?);
                }
                "audio" => {
                    if audio.is_some() {
                        return Err(de::Error::duplicate_field("audio"));
                    }
                    audio = Some(map.next_value()?);
                }
                "filters" => {
                    if filters.is_some() {
                        return Err(de::Error::duplicate_field("filters"));
                    }
                    // Present field decodes the validated chain directly. An
                    // explicit null is refused here (map-only), so only the
                    // absent key defaults to the empty chain.
                    filters = Some(map.next_value::<FilterChain>()?);
                }
                "renderer" => {
                    if renderer.is_some() {
                        return Err(de::Error::duplicate_field("renderer"));
                    }
                    // Present field decodes the validated renderer directly;
                    // an explicit null is refused (map-only), so only the
                    // absent key defaults to the baseline.
                    renderer = Some(map.next_value::<RendererSettings>()?);
                }
                other => return Err(de::Error::unknown_field(other, APPLIED_FIELDS)),
            }
        }

        Ok(AppliedV1 {
            video: video.ok_or_else(|| de::Error::missing_field("video"))?,
            audio: audio.ok_or_else(|| de::Error::missing_field("audio"))?,
            filters: filters.unwrap_or_default(),
            renderer: renderer.unwrap_or_default(),
        })
    }
}

impl<'de> Deserialize<'de> for PreferencesV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Not the `object_dto!` derive: `output` is absent-by-default for
        // backward compatibility, which a fully strict field set cannot
        // express. Everything else keeps the derive's strictness.
        deserializer.deserialize_struct("PreferencesV1", PREFS_FIELDS, PreferencesVisitor)
    }
}

struct PreferencesVisitor;

impl<'de> Visitor<'de> for PreferencesVisitor {
    type Value = PreferencesV1;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "a preferences object with volume_percent, muted, fullscreen and optional output and stats_hidden",
        )
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut volume_percent: Option<u8> = None;
        let mut muted: Option<bool> = None;
        let mut fullscreen: Option<bool> = None;
        let mut output: Option<Option<OutputV1>> = None;
        let mut stats_hidden: Option<bool> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "volume_percent" => {
                    if volume_percent.is_some() {
                        return Err(de::Error::duplicate_field("volume_percent"));
                    }
                    volume_percent = Some(map.next_value()?);
                }
                "muted" => {
                    if muted.is_some() {
                        return Err(de::Error::duplicate_field("muted"));
                    }
                    muted = Some(map.next_value()?);
                }
                "fullscreen" => {
                    if fullscreen.is_some() {
                        return Err(de::Error::duplicate_field("fullscreen"));
                    }
                    fullscreen = Some(map.next_value()?);
                }
                "output" => {
                    if output.is_some() {
                        return Err(de::Error::duplicate_field("output"));
                    }
                    // Present field deserializes `OutputV1` directly: an
                    // explicit null is refused (map-only visitor), only the
                    // absent key defaults to `Auto`.
                    output = Some(Some(map.next_value::<OutputV1>()?));
                }
                "stats_hidden" => {
                    if stats_hidden.is_some() {
                        return Err(de::Error::duplicate_field("stats_hidden"));
                    }
                    stats_hidden = Some(map.next_value::<bool>()?);
                }
                other => return Err(de::Error::unknown_field(other, PREFS_FIELDS)),
            }
        }

        Ok(PreferencesV1 {
            volume_percent: volume_percent
                .ok_or_else(|| de::Error::missing_field("volume_percent"))?,
            muted: muted.ok_or_else(|| de::Error::missing_field("muted"))?,
            fullscreen: fullscreen.ok_or_else(|| de::Error::missing_field("fullscreen"))?,
            output: output.flatten(),
            stats_hidden: stats_hidden.unwrap_or(false),
        })
    }
}

impl<'de> Deserialize<'de> for OutputV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // `deserialize_map`, not `deserialize_struct`: the tagged union must
        // never accept the sequence form or null, and the tag decides which
        // sibling fields are legal. The visitor's `expecting` names the shape.
        deserializer.deserialize_map(OutputVisitor)
    }
}

struct OutputVisitor;

impl<'de> Visitor<'de> for OutputVisitor {
    type Value = OutputV1;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an output choice object with a kind tag of auto or manual")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut kind: Option<String> = None;
        let mut sink: Option<SinkV1> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "kind" => {
                    if kind.is_some() {
                        return Err(de::Error::duplicate_field("kind"));
                    }
                    kind = Some(map.next_value()?);
                }
                "sink" => {
                    if sink.is_some() {
                        return Err(de::Error::duplicate_field("sink"));
                    }
                    sink = Some(map.next_value()?);
                }
                other => return Err(de::Error::unknown_field(other, OUTPUT_FIELDS)),
            }
        }

        let kind = kind.ok_or_else(|| de::Error::missing_field("kind"))?;
        match kind.as_str() {
            "auto" => {
                if sink.is_some() {
                    return Err(de::Error::unknown_field("sink", &[]));
                }
                Ok(OutputV1::Auto)
            }
            "manual" => Ok(OutputV1::Manual {
                sink: sink.ok_or_else(|| de::Error::missing_field("sink"))?,
            }),
            other => Err(de::Error::unknown_variant(other, &["auto", "manual"])),
        }
    }
}

impl<'de> Deserialize<'de> for FormatPolicyV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // `deserialize_str`, not a generic value: only the literal string
        // `"auto"` is a policy marker, so `null`, numbers and arrays are
        // rejected as invalid types rather than coerced.
        struct FormatPolicyVisitor;

        impl Visitor<'_> for FormatPolicyVisitor {
            type Value = FormatPolicyV1;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("the string \"auto\"")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "auto" => Ok(FormatPolicyV1::Auto),
                    other => Err(de::Error::unknown_variant(other, &["auto"])),
                }
            }
        }

        deserializer.deserialize_str(FormatPolicyVisitor)
    }
}

impl<'de> Deserialize<'de> for VideoV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Not the `object_dto!` derive: `format_policy` is absent-by-default for
        // backward compatibility, and a plain `Option` field would also accept
        // an explicit `null`. The visitor refuses `null` (the marker type is
        // string-only) while still defaulting only a missing key to manual.
        deserializer.deserialize_struct("VideoV1", VIDEO_FIELDS, VideoVisitor)
    }
}

struct VideoVisitor;

impl<'de> Visitor<'de> for VideoVisitor {
    type Value = VideoV1;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a video object with identity, mode and optional format_policy")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut identity: Option<DeviceIdentityV1> = None;
        let mut mode: Option<CaptureModeV1> = None;
        let mut format_policy: Option<FormatPolicyV1> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "identity" => {
                    if identity.is_some() {
                        return Err(de::Error::duplicate_field("identity"));
                    }
                    identity = Some(map.next_value()?);
                }
                "mode" => {
                    if mode.is_some() {
                        return Err(de::Error::duplicate_field("mode"));
                    }
                    mode = Some(map.next_value()?);
                }
                "format_policy" => {
                    if format_policy.is_some() {
                        return Err(de::Error::duplicate_field("format_policy"));
                    }
                    // Present field decodes the marker directly: an explicit
                    // `null` is refused, only the absent key defaults to the
                    // legacy manual pin.
                    format_policy = Some(map.next_value::<FormatPolicyV1>()?);
                }
                other => return Err(de::Error::unknown_field(other, VIDEO_FIELDS)),
            }
        }

        Ok(VideoV1 {
            identity: identity.ok_or_else(|| de::Error::missing_field("identity"))?,
            mode: mode.ok_or_else(|| de::Error::missing_field("mode"))?,
            format_policy,
        })
    }
}

impl<'de> Deserialize<'de> for DocumentV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_struct("DocumentV1", DOCUMENT_V1_FIELDS, DocumentVisitor)
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
                other => return Err(de::Error::unknown_field(other, DOCUMENT_V1_FIELDS)),
            }
        }

        let schema_version =
            schema_version.ok_or_else(|| de::Error::missing_field("schema_version"))?;
        let applied = applied.ok_or_else(|| de::Error::missing_field("applied"))?;
        let preferences = preferences.ok_or_else(|| de::Error::missing_field("preferences"))?;

        // Unreachable through `decode` (the probe gates first) but keeps the
        // visitor self-contained if it is ever reused.
        if schema_version != 1 {
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

impl<'de> Deserialize<'de> for DocumentV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_struct("DocumentV2", DOCUMENT_V2_FIELDS, DocumentV2Visitor)
    }
}

struct DocumentV2Visitor;

impl<'de> Visitor<'de> for DocumentV2Visitor {
    type Value = DocumentV2;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a version 2 source profile document object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut schema_version: Option<u64> = None;
        let mut profiles: Option<Vec<ProfileV2>> = None;
        let mut last_source: Option<Option<DeviceIdentityV1>> = None;
        let mut preferences: Option<PreferencesV1> = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "schema_version" => {
                    if schema_version.is_some() {
                        return Err(de::Error::duplicate_field("schema_version"));
                    }
                    schema_version = Some(map.next_value()?);
                }
                "profiles" => {
                    if profiles.is_some() {
                        return Err(de::Error::duplicate_field("profiles"));
                    }
                    profiles = Some(map.next_value()?);
                }
                "last_source" => {
                    if last_source.is_some() {
                        return Err(de::Error::duplicate_field("last_source"));
                    }
                    last_source = Some(map.next_value()?);
                }
                "preferences" => {
                    if preferences.is_some() {
                        return Err(de::Error::duplicate_field("preferences"));
                    }
                    preferences = Some(map.next_value()?);
                }
                other => return Err(de::Error::unknown_field(other, DOCUMENT_V2_FIELDS)),
            }
        }
        let version = schema_version.ok_or_else(|| de::Error::missing_field("schema_version"))?;
        if version != SCHEMA_VERSION {
            return Err(de::Error::invalid_value(
                de::Unexpected::Unsigned(version),
                &"schema_version 2",
            ));
        }
        Ok(DocumentV2 {
            profiles: profiles.ok_or_else(|| de::Error::missing_field("profiles"))?,
            last_source: last_source.ok_or_else(|| de::Error::missing_field("last_source"))?,
            preferences: preferences.ok_or_else(|| de::Error::missing_field("preferences"))?,
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
/// 3. strict version-specific shape, then validated domain conversion.
pub(crate) fn decode(bytes: &[u8]) -> Result<StoredDocument, SchemaError> {
    serde_json::from_slice::<IgnoredAny>(bytes).map_err(SchemaError::MalformedJson)?;

    let probe = serde_json::from_slice::<VersionProbe>(bytes).map_err(SchemaError::Schema)?;
    match probe.schema_version {
        1 => {
            let document =
                serde_json::from_slice::<DocumentV1>(bytes).map_err(SchemaError::Schema)?;
            stored_from_v1(document).map_err(SchemaError::InvalidValue)
        }
        SCHEMA_VERSION => {
            let document =
                serde_json::from_slice::<DocumentV2>(bytes).map_err(SchemaError::Schema)?;
            stored_from_v2(document).map_err(SchemaError::InvalidValue)
        }
        version => Err(SchemaError::UnsupportedVersion(version)),
    }
}

fn stored_from_v1(document: DocumentV1) -> Result<StoredDocument, SettingsValidationError> {
    let (profiles, last_source) = match document.applied {
        Some(applied) => {
            let settings = applied_from_dto(applied)?;
            let identity = settings.video.identity.clone();
            let binding = if identity.serial().is_some() {
                ProfileBinding::Serial
            } else {
                ProfileBinding::Topology
            };
            (
                SourceProfiles::new(vec![SourceProfile::new(binding, settings)?])?,
                Some(identity),
            )
        }
        None => (SourceProfiles::default(), None),
    };
    Ok(StoredDocument {
        profiles,
        last_source,
        preferences: preferences_from_dto(document.preferences)?,
    })
}

fn stored_from_v2(document: DocumentV2) -> Result<StoredDocument, SettingsValidationError> {
    let entries = document
        .profiles
        .into_iter()
        .map(|profile| {
            let binding = match profile.binding {
                BindingV2::Serial => ProfileBinding::Serial,
                BindingV2::Topology => ProfileBinding::Topology,
            };
            Ok(SourceProfile::new(
                binding,
                applied_from_dto(profile.settings)?,
            )?)
        })
        .collect::<Result<Vec<_>, SettingsValidationError>>()?;
    let profiles = SourceProfiles::new(entries)?;
    let last_source = document.last_source.map(identity_from_dto).transpose()?;
    profiles.validate(last_source.as_ref())?;
    Ok(StoredDocument {
        profiles,
        last_source,
        preferences: preferences_from_dto(document.preferences)?,
    })
}

// ---------------------------------------------------------------------------
// Encode pipeline
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DocumentWrite<'a> {
    schema_version: u64,
    profiles: ProfilesWrite<'a>,
    last_source: Option<DeviceIdentityWrite<'a>>,
    preferences: PreferencesWrite<'a>,
}

struct ProfilesWrite<'a>(&'a SourceProfiles);

impl Serialize for ProfilesWrite<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct ProfileWrite<'a> {
            binding: BindingV2,
            settings: AppliedWrite<'a>,
        }
        let mut sequence = serializer.serialize_seq(Some(self.0.entries().len()))?;
        for profile in self.0.entries() {
            let binding = match profile.binding() {
                ProfileBinding::Serial => BindingV2::Serial,
                ProfileBinding::Topology => BindingV2::Topology,
            };
            let settings = applied_to_dto(profile.settings());
            sequence.serialize_element(&ProfileWrite { binding, settings })?;
        }
        sequence.end()
    }
}

/// Borrowed preferences retain their existing optional output/stats markers.
/// `Auto` omits the output key; manual choices carry only the user's identity.
#[derive(Serialize)]
struct PreferencesWrite<'a> {
    volume_percent: u8,
    muted: bool,
    fullscreen: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<OutputWrite<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats_hidden: Option<bool>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum OutputWrite<'a> {
    Manual { sink: SinkWrite<'a> },
}

#[derive(Serialize)]
struct SinkWrite<'a> {
    name: &'a str,
    #[serde(serialize_with = "serialize_stable_properties")]
    stable_properties: &'a [(String, String)],
}

#[derive(Serialize)]
struct AppliedWrite<'a> {
    video: VideoWrite<'a>,
    audio: AudioWrite<'a>,
    /// Borrowed, validated chain; a nonempty all-disabled chain is preserved.
    #[serde(skip_serializing_if = "chain_is_empty")]
    filters: &'a FilterChain,
    /// Borrowed, validated renderer; every non-baseline value is written in full.
    #[serde(skip_serializing_if = "renderer_is_baseline")]
    renderer: &'a RendererSettings,
}

#[derive(Serialize)]
struct VideoWrite<'a> {
    identity: DeviceIdentityWrite<'a>,
    mode: CaptureModeV1,
    /// The existing payload convention: manual omits the marker, Auto records it.
    #[serde(skip_serializing_if = "Option::is_none")]
    format_policy: Option<FormatPolicyV1>,
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

/// `skip_serializing_if` predicate for the applied filter section: only a chain
/// with no entries is omitted.
fn chain_is_empty(chain: &FilterChain) -> bool {
    chain.entries().is_empty()
}

/// `skip_serializing_if` predicate for the applied renderer section: only the
/// exact behaviour-preserving baseline is omitted.
fn renderer_is_baseline(renderer: &RendererSettings) -> bool {
    renderer.is_baseline()
}

/// Validate public mutable preferences before serialization. Identity, topology,
/// source properties, size and rational rate are private constructor-validated
/// domain values: their borrowed wire views preserve those invariants without
/// rebuilding strings/vectors or sorting already-normalized properties.
pub(crate) fn encode(document: &StoredDocument) -> Result<Vec<u8>, EncodeError> {
    encode_profiles(
        &document.profiles,
        document.last_source.as_ref(),
        &document.preferences,
    )
}

pub(crate) fn encode_profiles(
    profiles: &SourceProfiles,
    last_source: Option<&DeviceIdentity>,
    preferences: &LocalPreferences,
) -> Result<Vec<u8>, EncodeError> {
    let preferences = preferences_to_dto(preferences)?;
    profiles
        .validate(last_source)
        .map_err(SettingsValidationError::from)?;
    // Public video requests are mutable, while each profile is privately held.
    // Validate the complete collection before serialization, preserving typed
    // errors for any invalid verified aggregate constructed by a caller.
    for profile in profiles.entries() {
        validate_video(&profile.settings().video)?;
    }
    let document = DocumentWrite {
        schema_version: SCHEMA_VERSION,
        profiles: ProfilesWrite(profiles),
        last_source: last_source.map(identity_to_dto),
        preferences,
    };
    let mut bytes = serde_json::to_vec(&document).map_err(EncodeError::Serialize)?;
    bytes.push(b'\n');
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Domain conversions
// ---------------------------------------------------------------------------

/// Borrowed wire views are built only after `encode` validates every profile.
fn applied_to_dto(applied: &DraftSettings) -> AppliedWrite<'_> {
    AppliedWrite {
        video: video_to_dto(&applied.video),
        audio: audio_to_dto(&applied.audio),
        filters: &applied.filters,
        renderer: &applied.renderer,
    }
}

fn applied_from_dto(applied: AppliedV1) -> Result<DraftSettings, SettingsValidationError> {
    Ok(DraftSettings {
        filters: applied.filters,
        video: video_from_dto(applied.video)?,
        audio: audio_from_dto(applied.audio)?,
        renderer: applied.renderer,
    })
}

fn validate_video(video: &VideoRequest) -> Result<(), SettingsValidationError> {
    if video.resolved_fourcc.is_none() {
        return Err(SettingsValidationError::UnresolvedVideo);
    }
    if video.resolved_mode().is_none() {
        return Err(SettingsValidationError::VideoFormatPolicyMismatch);
    }
    Ok(())
}

fn video_to_dto(video: &VideoRequest) -> VideoWrite<'_> {
    // The private writer is constructed only after the entire document passes
    // typed validation. Do not repeat resolution or allocate intermediate views.
    let concrete = video
        .resolved_fourcc
        .expect("document video validated before serialization");
    let format_policy = match video.mode.format_policy {
        InputFormatPolicy::Auto => Some(FormatPolicyV1::Auto),
        InputFormatPolicy::Manual { .. } => None,
    };
    VideoWrite {
        identity: identity_to_dto(&video.identity),
        mode: CaptureModeV1 {
            captured_fourcc: concrete.kernel_value(),
            width: video.mode.size.width(),
            height: video.mode.size.height(),
            fps: FrameRateV1 {
                numerator: video.mode.rate.numerator(),
                denominator: video.mode.rate.denominator(),
            },
        },
        format_policy,
    }
}

fn video_from_dto(video: VideoV1) -> Result<VideoRequest, SettingsValidationError> {
    let VideoV1 {
        identity,
        mode,
        format_policy,
    } = video;
    let identity = identity_from_dto(identity)?;

    // The stored concrete format is the proven opening. An absent marker is the
    // legacy manual pin; the `"auto"` marker keeps the historical concrete as
    // an audit value that startup clears before a fresh selection.
    let concrete = CapturedFourCc::from_kernel(mode.captured_fourcc);
    let format_policy = match format_policy {
        Some(FormatPolicyV1::Auto) => InputFormatPolicy::Auto,
        None => InputFormatPolicy::Manual { fourcc: concrete },
    };

    let request = VideoRequest {
        identity,
        mode: CaptureModeRequest {
            format_policy,
            size: FrameSize::new(mode.width, mode.height)?,
            rate: FrameRate::new(mode.fps.numerator, mode.fps.denominator)?,
        },
        resolved_fourcc: Some(concrete),
        selected_node: None,
    };
    // A structurally valid document can still describe no verified opening: the
    // automatic policy only ever resolves to YUYV/NV12/MJPG, so an on-disk auto
    // marker paired with a manual-only or unrecognized FourCC must be refused at
    // the same domain invariant that guards the encode side rather than loaded as
    // a poison baseline (startup Auto clearing repairs a live mode, not the store).
    validate_video(&request)?;
    Ok(request)
}

fn identity_to_dto(identity: &DeviceIdentity) -> DeviceIdentityWrite<'_> {
    DeviceIdentityWrite {
        vendor_id: identity.vendor_id(),
        product_id: identity.product_id(),
        topology: UsbTopologyWrite {
            controller: identity.topology().controller(),
            ports: identity.topology().ports(),
        },
        serial: identity.serial(),
    }
}

fn identity_from_dto(
    identity: DeviceIdentityV1,
) -> Result<DeviceIdentity, SettingsValidationError> {
    let topology = UsbTopology::new(identity.topology.controller, identity.topology.ports)?;
    Ok(DeviceIdentity::new(
        identity.vendor_id,
        identity.product_id,
        topology,
        identity.serial,
    )?)
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
    preferences: &LocalPreferences,
) -> Result<PreferencesWrite<'_>, SettingsValidationError> {
    let gain = PlaybackGain::new(preferences.gain.volume_percent, preferences.gain.muted)?;
    let output = match &preferences.output {
        PersistentOutputChoice::Auto => None,
        PersistentOutputChoice::Manual(identity) => Some(OutputWrite::Manual {
            sink: SinkWrite {
                name: identity.name(),
                stable_properties: identity.stable_properties(),
            },
        }),
    };
    Ok(PreferencesWrite {
        volume_percent: gain.volume_percent,
        muted: gain.muted,
        fullscreen: preferences.fullscreen,
        output,
        stats_hidden: preferences.stats_hidden.then_some(true),
    })
}

fn preferences_from_dto(
    preferences: PreferencesV1,
) -> Result<LocalPreferences, SettingsValidationError> {
    let output = match preferences.output {
        None | Some(OutputV1::Auto) => PersistentOutputChoice::Auto,
        Some(OutputV1::Manual { sink }) => {
            let properties = sink
                .stable_properties
                .into_iter()
                .map(|property| (property.key, property.value))
                .collect();
            PersistentOutputChoice::Manual(SinkIdentity::new(sink.name, properties)?)
        }
    };
    Ok(LocalPreferences {
        gain: PlaybackGain::new(preferences.volume_percent, preferences.muted)?,
        fullscreen: preferences.fullscreen,
        output,
        stats_hidden: preferences.stats_hidden,
    })
}
