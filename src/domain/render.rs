//! Responsibility: Closed, validated, serializable SDR renderer settings for the pinned
//! gpu-next/Vulkan build: scaler kernels, chroma/downscale choices, anti-ringing, dither,
//! deband and interpolation.
//! Allowed dependencies: pure types, `serde`/`serde_json` and `thiserror`; no Qt, and no
//! mpv pointers or native property names (those live in `media::render_catalog`).
//!
//! Bounds and defaults are product decisions grounded in the pinned stack: the mpv v0.41.0
//! renderer option inventory (`tests/fixtures/FUR-002/common/probes/mpv_renderer_option_help.stdout`,
//! 39 announced kernels, `deband` flag, `deband-iterations` `0..=16`, `deband-grain` `0..=4096`,
//! anti-ringing `0..=1`, `dither` in `fruit|ordered|error-diffusion|no`). They document
//! inclusive intervals and defaults; they are not proof that every kernel or the extreme
//! endpoints were functionally exercised. Whether an announced kernel is *effective* on the
//! qualified gpu-next build is a capability decision made by the media catalogue, never
//! inferred here: four announced kernels fall back to `bilinear` there and are rejected before
//! mutation, yet remain recognized domain values so the failure can be typed.
//!
//! Serialization shape (validated on decode, never bypassing a constructor):
//!
//! ```json
//! {"scale":"ewa_lanczossharp","cscale":"ewa_lanczossharp","dscale":"mitchell",
//!  "scale_antiring":0.7,"dither":"error-diffusion",
//!  "deband":{"enabled":true,"iterations":2,"grain":16},"interpolation":false}
//! ```
//!
//! The object is complete and strict: unknown, missing, duplicate and null fields, arrays and
//! non-string kernel tokens are rejected. `cscale`/`dscale` accept `"auto"` (inherit, an empty
//! native value); `scale` has no auto and always names a kernel. Deband parameters are
//! validated even when `deband.enabled` is false, and a negative zero is normalized to `+0.0`.

use std::{fmt, marker::PhantomData};

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// Require an object before delegating named-field checks to Serde, whose
/// derived struct deserializer would also accept positional sequences.
fn deserialize_object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct ObjectVisitor<T>(PhantomData<T>);

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

    deserializer.deserialize_map(ObjectVisitor(PhantomData))
}

// ---------------------------------------------------------------------------
// Scaler kernels
// ---------------------------------------------------------------------------

macro_rules! scaler_kernels {
    ($( ($variant:ident, $token:literal) ),* $(,)?) => {
        /// A gpu-next scaler kernel, named by the exact token announced by the pinned build.
        ///
        /// Recognition here is independent of effectiveness: a kernel the catalogue proves is
        /// a no-op fallback is still a valid domain value so the media layer can return a typed
        /// `UnsupportedValue` instead of accepting a silent fallback.
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
        pub enum ScalerKernel {
            $(
                #[doc = concat!("`", $token, "`.")]
                $variant,
            )*
        }

        impl ScalerKernel {
            /// Every announced kernel, in the pinned inventory order (39 values).
            pub const ALL: &[ScalerKernel] = &[ $( ScalerKernel::$variant ),* ];

            /// The exact serialized token for this kernel.
            pub fn as_str(self) -> &'static str {
                match self {
                    $( ScalerKernel::$variant => $token, )*
                }
            }

            /// Parse the exact serialized token, or `None` for an unknown kernel.
            pub fn from_token(token: &str) -> Option<Self> {
                match token {
                    $( $token => Some(ScalerKernel::$variant), )*
                    _ => None,
                }
            }
        }
    };
}

scaler_kernels! {
    (Bilinear, "bilinear"),
    (BicubicFast, "bicubic_fast"),
    (Oversample, "oversample"),
    (Spline16, "spline16"),
    (Spline36, "spline36"),
    (Spline64, "spline64"),
    (Sinc, "sinc"),
    (Lanczos, "lanczos"),
    (Ginseng, "ginseng"),
    (Bicubic, "bicubic"),
    (Hermite, "hermite"),
    (CatmullRom, "catmull_rom"),
    (Mitchell, "mitchell"),
    (Robidoux, "robidoux"),
    (RobidouxSharp, "robidouxsharp"),
    (Box, "box"),
    (Nearest, "nearest"),
    (Triangle, "triangle"),
    (Gaussian, "gaussian"),
    (Jinc, "jinc"),
    (EwaLanczos, "ewa_lanczos"),
    (EwaHanning, "ewa_hanning"),
    (EwaGinseng, "ewa_ginseng"),
    (EwaLanczosSharp, "ewa_lanczossharp"),
    (EwaLanczos4Sharpest, "ewa_lanczos4sharpest"),
    (EwaLanczosSoft, "ewa_lanczossoft"),
    (HaasnSoft, "haasnsoft"),
    (EwaRobidoux, "ewa_robidoux"),
    (EwaRobidouxSharp, "ewa_robidouxsharp"),
    (Bartlett, "bartlett"),
    (Cosine, "cosine"),
    (Hanning, "hanning"),
    (Tukey, "tukey"),
    (Hamming, "hamming"),
    (Quadric, "quadric"),
    (Welch, "welch"),
    (Kaiser, "kaiser"),
    (Blackman, "blackman"),
    (Sphinx, "sphinx"),
}

impl Serialize for ScalerKernel {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ScalerKernel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct KernelVisitor;

        impl Visitor<'_> for KernelVisitor {
            type Value = ScalerKernel;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a scaler kernel name")
            }

            fn visit_str<E>(self, value: &str) -> Result<ScalerKernel, E>
            where
                E: de::Error,
            {
                ScalerKernel::from_token(value)
                    .ok_or_else(|| E::custom(format_args!("unknown scaler kernel `{value}`")))
            }
        }

        deserializer.deserialize_str(KernelVisitor)
    }
}

/// A scaler selection: an explicit kernel or `"auto"` (inherit, clearing any prior kernel).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ScalerChoice {
    /// Inherit the upstream/previous value (native empty string).
    Inherit,
    /// An explicit kernel.
    Kernel(ScalerKernel),
}

impl Serialize for ScalerChoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ScalerChoice::Inherit => serializer.serialize_str("auto"),
            ScalerChoice::Kernel(kernel) => serializer.serialize_str(kernel.as_str()),
        }
    }
}

impl<'de> Deserialize<'de> for ScalerChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ChoiceVisitor;

        impl Visitor<'_> for ChoiceVisitor {
            type Value = ScalerChoice;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("`auto` or a scaler kernel name")
            }

            fn visit_str<E>(self, value: &str) -> Result<ScalerChoice, E>
            where
                E: de::Error,
            {
                if value == "auto" {
                    return Ok(ScalerChoice::Inherit);
                }
                ScalerKernel::from_token(value)
                    .map(ScalerChoice::Kernel)
                    .ok_or_else(|| E::custom(format_args!("unknown scaler kernel `{value}`")))
            }
        }

        deserializer.deserialize_str(ChoiceVisitor)
    }
}

// ---------------------------------------------------------------------------
// Dither
// ---------------------------------------------------------------------------

/// Dither algorithm. The four values announced by the pinned build.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum DitherMode {
    /// Error-free dithering (the pinned default).
    #[serde(rename = "fruit")]
    Fruit,
    /// Ordered dithering.
    #[serde(rename = "ordered")]
    Ordered,
    /// Error-diffusion dithering (GPU-memory dependent).
    #[serde(rename = "error-diffusion")]
    ErrorDiffusion,
    /// Disable dithering.
    #[serde(rename = "no")]
    No,
}

impl DitherMode {
    /// Every dither algorithm, in inventory order.
    pub const ALL: &[DitherMode] = &[
        DitherMode::Fruit,
        DitherMode::Ordered,
        DitherMode::ErrorDiffusion,
        DitherMode::No,
    ];

    /// The exact serialized token for this dither.
    pub fn as_str(self) -> &'static str {
        match self {
            DitherMode::Fruit => "fruit",
            DitherMode::Ordered => "ordered",
            DitherMode::ErrorDiffusion => "error-diffusion",
            DitherMode::No => "no",
        }
    }
}

// ---------------------------------------------------------------------------
// Validated error
// ---------------------------------------------------------------------------

/// A renderer property that can be named in a validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize)]
pub enum RendererProperty {
    /// Luma upscaling kernel (`scale`).
    Scale,
    /// Chroma upscaling kernel (`cscale`).
    Cscale,
    /// Luma downscaling kernel (`dscale`).
    Dscale,
    /// Anti-ringing strength (`scale-antiring`).
    ScaleAntiring,
    /// Dither algorithm (`dither`).
    Dither,
    /// Deband enable flag (`deband`).
    Deband,
    /// Deband iteration count (`deband-iterations`).
    DebandIterations,
    /// Deband grain strength (`deband-grain`).
    DebandGrain,
    /// Interpolation enable flag (`interpolation`).
    Interpolation,
}

impl RendererProperty {
    /// Every renderer property, in the emission order used by the owner thread.
    pub const ALL: &[RendererProperty] = &[
        RendererProperty::Scale,
        RendererProperty::Cscale,
        RendererProperty::Dscale,
        RendererProperty::ScaleAntiring,
        RendererProperty::Dither,
        RendererProperty::Deband,
        RendererProperty::DebandIterations,
        RendererProperty::DebandGrain,
        RendererProperty::Interpolation,
    ];

    /// Stable snake_case identifier used for diagnostics (never a native option name).
    pub fn as_str(self) -> &'static str {
        match self {
            RendererProperty::Scale => "scale",
            RendererProperty::Cscale => "cscale",
            RendererProperty::Dscale => "dscale",
            RendererProperty::ScaleAntiring => "scale_antiring",
            RendererProperty::Dither => "dither",
            RendererProperty::Deband => "deband",
            RendererProperty::DebandIterations => "deband_iterations",
            RendererProperty::DebandGrain => "deband_grain",
            RendererProperty::Interpolation => "interpolation",
        }
    }
}

impl fmt::Display for RendererProperty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a renderer value was rejected.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RenderValidationReason {
    /// A numeric value was NaN or infinite.
    #[error("value {value} is not finite")]
    NonFinite {
        /// Rejected value.
        value: f64,
    },
    /// A finite value fell outside its inclusive interval.
    #[error("value {value} is outside [{min}, {max}]")]
    OutOfRange {
        /// Rejected value.
        value: f64,
        /// Inclusive minimum.
        min: f64,
        /// Inclusive maximum.
        max: f64,
    },
}

/// Invalid renderer data: the offending property plus a typed reason.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("renderer property `{property}`: {reason}")]
pub struct RenderValidationError {
    /// The property that was rejected.
    pub property: RendererProperty,
    /// Why it was rejected.
    pub reason: RenderValidationReason,
}

// ---------------------------------------------------------------------------
// Input / validated records
// ---------------------------------------------------------------------------

/// Raw deband inputs, validated by [`RendererSettings::new`]. Signed zero grain is normalized.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct DebandValues {
    /// Whether debanding is enabled.
    pub enabled: bool,
    /// Iteration count, inclusive `0..=16`.
    pub iterations: u8,
    /// Grain strength, inclusive `0..=4096`.
    pub grain: f64,
}

/// Raw renderer inputs, validated by [`RendererSettings::new`]. Never stored directly.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct RendererValues {
    /// Luma upscaling kernel.
    pub scale: ScalerKernel,
    /// Chroma upscaling kernel or `auto`.
    pub cscale: ScalerChoice,
    /// Luma downscaling kernel or `auto`.
    pub dscale: ScalerChoice,
    /// Anti-ringing strength, inclusive `0..=1`.
    pub scale_antiring: f64,
    /// Dither algorithm.
    pub dither: DitherMode,
    /// Deband parameters, always validated (even when disabled).
    pub deband: DebandValues,
    /// Interpolation flag.
    ///
    /// Persisted and applied, but inactive under the current low-latency timing; that is a
    /// product limitation surfaced to the user, not a reason to reject the value.
    pub interpolation: bool,
}

/// Validated SDR renderer settings. Constructed only through [`RendererSettings::new`] (or
/// [`Default`], which builds the baseline); the raw values are private.
#[derive(Clone, Debug, PartialEq)]
pub struct RendererSettings {
    values: RendererValues,
}

impl Default for RendererSettings {
    /// The behaviour-preserving baseline: `lanczos`/`auto`/`hermite`/`0`/`fruit`/
    /// `{enabled:false, iterations:1, grain:32}`/`interpolation:false`.
    fn default() -> Self {
        RendererSettings::new(RendererValues {
            scale: ScalerKernel::Lanczos,
            cscale: ScalerChoice::Inherit,
            dscale: ScalerChoice::Kernel(ScalerKernel::Hermite),
            scale_antiring: 0.0,
            dither: DitherMode::Fruit,
            deband: DebandValues {
                enabled: false,
                iterations: 1,
                grain: 32.0,
            },
            interpolation: false,
        })
        .expect("renderer baseline is valid")
    }
}

impl RendererSettings {
    /// Validate and build renderer settings. Rejects non-finite numerics and out-of-interval
    /// anti-ringing (`0..=1`), iterations (`0..=16`) and grain (`0..=4096`); negative zero is
    /// normalized to positive zero. Deband parameters are validated even when disabled.
    pub fn new(values: RendererValues) -> Result<Self, RenderValidationError> {
        let scale_antiring = bounded(
            RendererProperty::ScaleAntiring,
            values.scale_antiring,
            0.0,
            1.0,
        )?;
        if values.deband.iterations > 16 {
            return Err(RenderValidationError {
                property: RendererProperty::DebandIterations,
                reason: RenderValidationReason::OutOfRange {
                    value: f64::from(values.deband.iterations),
                    min: 0.0,
                    max: 16.0,
                },
            });
        }
        let grain = bounded(
            RendererProperty::DebandGrain,
            values.deband.grain,
            0.0,
            4096.0,
        )?;
        Ok(Self {
            values: RendererValues {
                scale_antiring,
                deband: DebandValues {
                    enabled: values.deband.enabled,
                    iterations: values.deband.iterations,
                    grain,
                },
                ..values
            },
        })
    }

    /// The validated values.
    pub fn values(&self) -> RendererValues {
        self.values
    }

    /// Whether these settings are exactly the baseline (used to omit them from persistence).
    pub fn is_baseline(&self) -> bool {
        let values = self.values;
        values.scale == ScalerKernel::Lanczos
            && values.cscale == ScalerChoice::Inherit
            && values.dscale == ScalerChoice::Kernel(ScalerKernel::Hermite)
            && values.scale_antiring == 0.0
            && values.dither == DitherMode::Fruit
            && values.deband
                == (DebandValues {
                    enabled: false,
                    iterations: 1,
                    grain: 32.0,
                })
            && !values.interpolation
    }
}

/// Reject non-finite values first, then enforce an inclusive interval, then normalize a
/// negative zero to a positive zero. Never clamps.
fn bounded(
    property: RendererProperty,
    value: f64,
    min: f64,
    max: f64,
) -> Result<f64, RenderValidationError> {
    if !value.is_finite() {
        return Err(RenderValidationError {
            property,
            reason: RenderValidationReason::NonFinite { value },
        });
    }
    if value < min || value > max {
        return Err(RenderValidationError {
            property,
            reason: RenderValidationReason::OutOfRange { value, min, max },
        });
    }
    Ok(if value == 0.0 { 0.0 } else { value })
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

impl Serialize for RendererSettings {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.values.serialize(serializer)
    }
}

/// Strict object-only fields for the nested `deband` object, validated by
/// [`RendererSettings::new`] after decoding.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DebandFields {
    enabled: bool,
    iterations: u8,
    grain: f64,
}

/// Route the nested `deband` object through the strict object-only helper so positional
/// sequences are rejected, matching the top-level object.
fn deserialize_deband<'de, D>(deserializer: D) -> Result<DebandFields, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_object(deserializer)
}

impl<'de> Deserialize<'de> for RendererSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            scale: ScalerKernel,
            cscale: ScalerChoice,
            dscale: ScalerChoice,
            scale_antiring: f64,
            dither: DitherMode,
            #[serde(deserialize_with = "deserialize_deband")]
            deband: DebandFields,
            interpolation: bool,
        }

        let fields: Fields = deserialize_object(deserializer)?;
        RendererSettings::new(RendererValues {
            scale: fields.scale,
            cscale: fields.cscale,
            dscale: fields.dscale,
            scale_antiring: fields.scale_antiring,
            dither: fields.dither,
            deband: DebandValues {
                enabled: fields.deband.enabled,
                iterations: fields.deband.iterations,
                grain: fields.deband.grain,
            },
            interpolation: fields.interpolation,
        })
        .map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFERENCE_JSON: &str =
        include_str!("../../tests/fixtures/FUR-016/reference-renderer.json");

    fn baseline() -> RendererValues {
        RendererSettings::default().values()
    }

    fn reference() -> RendererSettings {
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

    fn assert_rejected(json: &str) {
        assert!(serde_json::from_str::<RendererSettings>(json).is_err());
    }

    fn reject(values: RendererValues) -> (RendererProperty, RenderValidationReason) {
        match RendererSettings::new(values) {
            Ok(_) => panic!("expected rejection"),
            Err(RenderValidationError { property, reason }) => (property, reason),
        }
    }

    fn out_of_range(values: RendererValues) -> RendererProperty {
        let (property, reason) = reject(values);
        assert!(matches!(reason, RenderValidationReason::OutOfRange { .. }));
        property
    }

    fn non_finite(values: RendererValues) -> RendererProperty {
        let (property, reason) = reject(values);
        assert!(matches!(reason, RenderValidationReason::NonFinite { .. }));
        property
    }

    fn with_antiring(value: f64) -> RendererValues {
        RendererValues {
            scale_antiring: value,
            ..baseline()
        }
    }

    fn with_deband(deband: DebandValues) -> RendererValues {
        RendererValues {
            deband,
            ..baseline()
        }
    }

    fn disabled_deband(iterations: u8, grain: f64) -> DebandValues {
        DebandValues {
            enabled: false,
            iterations,
            grain,
        }
    }

    // --- baseline -----------------------------------------------------------

    #[test]
    fn default_is_the_documented_baseline() {
        let settings = RendererSettings::default();
        assert_eq!(settings.values(), baseline());
        let v = settings.values();
        assert_eq!(v.scale, ScalerKernel::Lanczos);
        assert_eq!(v.cscale, ScalerChoice::Inherit);
        assert_eq!(v.dscale, ScalerChoice::Kernel(ScalerKernel::Hermite));
        assert_eq!(v.scale_antiring, 0.0);
        assert_eq!(v.dither, DitherMode::Fruit);
        assert_eq!(
            v.deband,
            DebandValues {
                enabled: false,
                iterations: 1,
                grain: 32.0
            }
        );
        assert!(!v.interpolation);
        assert!(settings.is_baseline());
        assert!(!reference().is_baseline());
    }

    // --- numeric bounds -----------------------------------------------------

    #[test]
    fn anti_ringing_enforces_inclusive_interval_and_finiteness() {
        for value in [0.0, 0.5, 1.0] {
            let values = with_antiring(value);
            assert_eq!(
                RendererSettings::new(values)
                    .unwrap()
                    .values()
                    .scale_antiring,
                value
            );
        }
        for value in [-0.0001, 1.0001, 2.0, -1.0] {
            assert_eq!(
                out_of_range(with_antiring(value)),
                RendererProperty::ScaleAntiring
            );
        }
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                non_finite(with_antiring(value)),
                RendererProperty::ScaleAntiring
            );
        }
    }

    #[test]
    fn deband_grain_enforces_inclusive_interval_and_finiteness() {
        for value in [0.0, 16.0, 4096.0] {
            let values = with_deband(DebandValues {
                grain: value,
                ..baseline().deband
            });
            assert_eq!(
                RendererSettings::new(values).unwrap().values().deband.grain,
                value
            );
        }
        for value in [-0.0001, 4096.0001, 99999.0] {
            assert_eq!(
                out_of_range(with_deband(DebandValues {
                    grain: value,
                    ..baseline().deband
                })),
                RendererProperty::DebandGrain
            );
        }
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                non_finite(with_deband(DebandValues {
                    grain: value,
                    ..baseline().deband
                })),
                RendererProperty::DebandGrain
            );
        }
    }

    #[test]
    fn deband_iterations_enforce_inclusive_interval() {
        for value in [0u8, 1, 8, 16] {
            let values = with_deband(DebandValues {
                iterations: value,
                ..baseline().deband
            });
            assert_eq!(
                RendererSettings::new(values)
                    .unwrap()
                    .values()
                    .deband
                    .iterations,
                value
            );
        }
        for value in [17u8, 32, 255] {
            assert_eq!(
                out_of_range(with_deband(DebandValues {
                    iterations: value,
                    ..baseline().deband
                })),
                RendererProperty::DebandIterations
            );
        }
    }

    #[test]
    fn negative_zero_is_normalized_to_positive() {
        let values = RendererValues {
            scale_antiring: -0.0,
            deband: DebandValues {
                grain: -0.0,
                ..baseline().deband
            },
            ..baseline()
        };
        let settings = RendererSettings::new(values).unwrap();
        assert_eq!(settings.values().scale_antiring.to_bits(), 0.0f64.to_bits());
        assert_eq!(settings.values().deband.grain.to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn disabled_deband_parameters_are_still_validated() {
        let bad_iterations = with_deband(disabled_deband(17, 16.0));
        assert_eq!(
            out_of_range(bad_iterations),
            RendererProperty::DebandIterations
        );
        let bad_grain = with_deband(disabled_deband(2, f64::INFINITY));
        assert_eq!(non_finite(bad_grain), RendererProperty::DebandGrain);
    }

    // --- scaler kernels and choices ----------------------------------------

    #[test]
    fn every_kernel_token_roundtrips_including_recognized_fallbacks() {
        for &kernel in ScalerKernel::ALL {
            let json = serde_json::to_string(&kernel).unwrap();
            assert_eq!(json, format!("\"{}\"", kernel.as_str()));
            assert_eq!(serde_json::from_str::<ScalerKernel>(&json).unwrap(), kernel);
        }
        // The four fallback kernels stay recognized domain values (media rejects them).
        for kernel in [
            ScalerKernel::EwaLanczosSoft,
            ScalerKernel::HaasnSoft,
            ScalerKernel::Bartlett,
            ScalerKernel::Tukey,
        ] {
            assert!(ScalerKernel::from_token(kernel.as_str()).is_some());
        }
        assert_eq!(ScalerKernel::ALL.len(), 39);
    }

    #[test]
    fn unknown_kernel_token_rejected() {
        assert!(ScalerKernel::from_token("bogus").is_none());
        assert!(serde_json::from_str::<ScalerKernel>("\"bogus\"").is_err());
        assert!(serde_json::from_str::<ScalerKernel>("42").is_err());
        assert!(serde_json::from_str::<ScalerKernel>("null").is_err());
    }

    #[test]
    fn scaler_choice_auto_is_inherit_and_scale_has_no_auto() {
        assert_eq!(
            serde_json::to_string(&ScalerChoice::Inherit).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::from_str::<ScalerChoice>("\"auto\"").unwrap(),
            ScalerChoice::Inherit
        );
        assert_eq!(
            serde_json::to_string(&ScalerChoice::Kernel(ScalerKernel::Hermite)).unwrap(),
            "\"hermite\""
        );

        // `auto` is valid for cscale/dscale but never for scale.
        let auto_cscale = r#"{"scale":"lanczos","cscale":"auto","dscale":"auto","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false}"#;
        let settings = serde_json::from_str::<RendererSettings>(auto_cscale).unwrap();
        assert_eq!(settings.values().cscale, ScalerChoice::Inherit);
        assert_eq!(settings.values().dscale, ScalerChoice::Inherit);

        let auto_scale = r#"{"scale":"auto","cscale":"auto","dscale":"auto","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false}"#;
        assert!(serde_json::from_str::<RendererSettings>(auto_scale).is_err());
    }

    #[test]
    fn every_dither_roundtrips() {
        for &dither in DitherMode::ALL {
            let json = serde_json::to_string(&dither).unwrap();
            assert_eq!(json, format!("\"{}\"", dither.as_str()));
            assert_eq!(serde_json::from_str::<DitherMode>(&json).unwrap(), dither);
        }
        assert_eq!(DitherMode::ErrorDiffusion.as_str(), "error-diffusion");
        assert!(serde_json::from_str::<DitherMode>("\"none\"").is_err());
    }

    // --- reference fixture --------------------------------------------------

    #[test]
    fn reference_fixture_parses_to_exact_expected_values() {
        let settings = serde_json::from_str::<RendererSettings>(REFERENCE_JSON).unwrap();
        assert_eq!(settings, reference());
        assert!(!settings.is_baseline());
        let v = settings.values();
        assert_eq!(v.scale, ScalerKernel::EwaLanczosSharp);
        assert_eq!(
            v.cscale,
            ScalerChoice::Kernel(ScalerKernel::EwaLanczosSharp)
        );
        assert_eq!(v.dscale, ScalerChoice::Kernel(ScalerKernel::Mitchell));
        assert_eq!(v.scale_antiring, 0.7);
        assert_eq!(v.dither, DitherMode::ErrorDiffusion);
        assert_eq!(
            v.deband,
            DebandValues {
                enabled: true,
                iterations: 2,
                grain: 16.0
            }
        );
        assert!(!v.interpolation);
    }

    #[test]
    fn reference_roundtrips_through_serialization() {
        let settings = reference();
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(
            serde_json::from_str::<RendererSettings>(&json).unwrap(),
            settings
        );
    }

    #[test]
    fn serialized_shape_is_flat_with_nested_deband() {
        let value = serde_json::to_value(reference()).unwrap();
        assert_eq!(value["scale"], "ewa_lanczossharp");
        assert_eq!(value["cscale"], "ewa_lanczossharp");
        assert_eq!(value["dscale"], "mitchell");
        assert_eq!(value["scale_antiring"], 0.7);
        assert_eq!(value["dither"], "error-diffusion");
        assert_eq!(value["deband"]["enabled"], true);
        assert_eq!(value["deband"]["iterations"], 2);
        assert_eq!(value["deband"]["grain"], 16.0);
        assert_eq!(value["interpolation"], false);
        assert!(value.get("values").is_none());
    }

    // --- strict decoding ----------------------------------------------------

    #[test]
    fn unknown_missing_null_and_array_fields_rejected() {
        for json in [
            // unknown top-level field
            r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false,"extra":1}"#,
            // unknown nested deband field
            r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32,"extra":1},"interpolation":false}"#,
            // missing top-level field
            r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32}}"#,
            // missing nested field
            r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1},"interpolation":false}"#,
            // explicit null for a field
            r#"{"scale":null,"cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false}"#,
            // explicit null for deband
            r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":null,"interpolation":false}"#,
            // positional array instead of object
            r#"["lanczos","auto","hermite",0,"fruit",{"enabled":false,"iterations":1,"grain":32},false]"#,
            // array for the nested deband object
            r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":[false,1,32],"interpolation":false}"#,
        ] {
            assert_rejected(json);
        }
    }

    #[test]
    fn duplicate_fields_rejected_in_either_order() {
        // A later duplicate must not silently win over an earlier value.
        for json in [
            r#"{"scale":"lanczos","scale":"ewa_lanczossharp","cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false}"#,
            r#"{"scale":"ewa_lanczossharp","scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false}"#,
            r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0.7,"scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"grain":32},"interpolation":false}"#,
            r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":1,"iterations":2,"grain":32},"interpolation":false}"#,
        ] {
            assert_rejected(json);
        }
    }

    #[test]
    fn invalid_present_deband_is_not_replaced_by_baseline() {
        // A present-but-invalid deband must never be replaced by baseline values.
        let json = r#"{"scale":"lanczos","cscale":"auto","dscale":"hermite","scale_antiring":0,"dither":"fruit","deband":{"enabled":false,"iterations":99,"grain":32},"interpolation":false}"#;
        assert_rejected(json);
    }
}
