//! Responsibility: Closed, validated, serializable filter catalog: exactly five product
//! filters (`format` native plus `eq`, `unsharp`, `hqdn3d`, `bwdif` via lavfi), typed
//! parameter payloads, an ordered label-unique chain and atomic editing operations.
//! Allowed dependencies: pure types, `serde`/`serde_json` and `thiserror`; no Qt, no
//! mpv/FFmpeg pointers, and no filter syntax (that lives in `media::filter_catalog`).
//!
//! Bounds and units are product decisions grounded in the pinned stack: the mpv v0.41.0
//! native filter documentation (<https://github.com/mpv-player/mpv/blob/v0.41.0/DOCS/man/vf.rst>)
//! and the FFmpeg n8.1.2 filter sources
//! (<https://github.com/FFmpeg/FFmpeg/tree/n8.1.2/libavfilter>). They document effective
//! intervals; they are not proof that the extreme endpoints were functionally exercised.
//!
//! Serialization shape (validated on decode, never bypassing a constructor):
//!
//! ```json
//! {"entries":[{"label":"Levels","enabled":true,"filter":{"kind":"eq","parameters":{...}}}]}
//! ```
//!
//! Unknown fields, duplicate fields, unknown filter kinds, unknown enum variants, missing
//! fields and invalid nested parameters are rejected. A display label is inert text: it is
//! only compared for exact uniqueness and is never interpreted as syntax.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// Closed identities
// ---------------------------------------------------------------------------

/// The five product filters. `Filter::kind()` is an exhaustive match, so a kind can never
/// disagree with its payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FilterKind {
    /// mpv-native `format` (SDR matrix/levels/gamma).
    #[serde(rename = "format")]
    Format,
    /// FFmpeg `eq` via lavfi.
    #[serde(rename = "eq")]
    Eq,
    /// FFmpeg `unsharp` via lavfi.
    #[serde(rename = "unsharp")]
    Unsharp,
    /// FFmpeg `hqdn3d` via lavfi.
    #[serde(rename = "hqdn3d")]
    Hqdn3d,
    /// FFmpeg `bwdif` via lavfi.
    #[serde(rename = "bwdif")]
    Bwdif,
}

impl FilterKind {
    /// Every product filter kind, in declaration order. This is the complete catalog.
    pub const ALL: [FilterKind; 5] = [
        FilterKind::Format,
        FilterKind::Eq,
        FilterKind::Unsharp,
        FilterKind::Hqdn3d,
        FilterKind::Bwdif,
    ];

    /// Stable lowercase identifier used for diagnostics (never filter syntax).
    pub fn as_str(self) -> &'static str {
        match self {
            FilterKind::Format => "format",
            FilterKind::Eq => "eq",
            FilterKind::Unsharp => "unsharp",
            FilterKind::Hqdn3d => "hqdn3d",
            FilterKind::Bwdif => "bwdif",
        }
    }

    /// Where this filter comes from: mpv itself or the linked FFmpeg via the lavfi bridge.
    pub fn origin(self) -> FilterOrigin {
        match self {
            FilterKind::Format => FilterOrigin::MpvNative,
            FilterKind::Eq | FilterKind::Unsharp | FilterKind::Hqdn3d | FilterKind::Bwdif => {
                FilterOrigin::FfmpegLavfi
            }
        }
    }
}

impl fmt::Display for FilterKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Origin of a filter registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum FilterOrigin {
    /// Registered by mpv itself.
    MpvNative,
    /// Registered by the linked FFmpeg library and reached through the lavfi bridge.
    FfmpegLavfi,
}

// ---------------------------------------------------------------------------
// Typed validation error
// ---------------------------------------------------------------------------

/// Invalid filter data or invalid editing request. Distinct from a missing-build capability.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FilterValidationError {
    /// A numeric parameter was NaN or infinite.
    #[error("filter `{filter}` parameter `{parameter}` is not finite: {value}")]
    NonFinite {
        /// Affected filter.
        filter: FilterKind,
        /// Parameter name.
        parameter: &'static str,
        /// Rejected value.
        value: f64,
    },
    /// A finite numeric parameter fell outside its inclusive interval.
    #[error("filter `{filter}` parameter `{parameter}` value {value} is outside [{min}, {max}]")]
    OutOfBounds {
        /// Affected filter.
        filter: FilterKind,
        /// Parameter name.
        parameter: &'static str,
        /// Rejected value.
        value: f64,
        /// Inclusive minimum.
        min: f64,
        /// Inclusive maximum.
        max: f64,
    },
    /// A matrix dimension was not an odd integer in `3..=23`.
    #[error(
        "filter `{filter}` parameter `{parameter}` matrix size {value} is not an odd value in 3..=23"
    )]
    InvalidMatrixSize {
        /// Affected filter.
        filter: FilterKind,
        /// Parameter name.
        parameter: &'static str,
        /// Rejected dimension.
        value: u32,
    },
    /// A label collided with an existing entry (exact `String` equality, including disabled
    /// entries).
    #[error(
        "duplicate label {label:?}: first used at index {first}, collided at index {duplicate}"
    )]
    DuplicateLabel {
        /// Offending label.
        label: String,
        /// Index of the first entry already using it.
        first: usize,
        /// Index of the colliding entry (or insertion point).
        duplicate: usize,
    },
    /// An index was outside the permitted range for the current chain length.
    #[error("index {index} is out of bounds for length {len}")]
    InvalidIndex {
        /// Rejected index.
        index: usize,
        /// Current number of entries (for `insert`, the allowed exclusive upper bound + 1).
        len: usize,
    },
}

// ---------------------------------------------------------------------------
// Numeric helpers
// ---------------------------------------------------------------------------

/// Reject non-finite values first, then enforce an inclusive interval, then normalize a
/// negative zero to a positive zero. Never clamps.
fn bounded(
    filter: FilterKind,
    parameter: &'static str,
    value: f64,
    min: f64,
    max: f64,
) -> Result<f64, FilterValidationError> {
    if !value.is_finite() {
        return Err(FilterValidationError::NonFinite {
            filter,
            parameter,
            value,
        });
    }
    if value < min || value > max {
        return Err(FilterValidationError::OutOfBounds {
            filter,
            parameter,
            value,
            min,
            max,
        });
    }
    Ok(if value == 0.0 { 0.0 } else { value })
}

/// Validate a matrix dimension as an odd integer in `3..=23` and narrow it to `u8`.
fn odd_matrix_size(
    filter: FilterKind,
    parameter: &'static str,
    value: u32,
) -> Result<u8, FilterValidationError> {
    if !(3..=23).contains(&value) || value.is_multiple_of(2) {
        return Err(FilterValidationError::InvalidMatrixSize {
            filter,
            parameter,
            value,
        });
    }
    Ok(value as u8)
}

// ---------------------------------------------------------------------------
// Input records (unvalidated constructor inputs; never stored, never accepted by media)
// ---------------------------------------------------------------------------

/// Raw `eq` inputs, validated by [`EqParams::new`]. Signed zero is normalized to `+0.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqValues {
    /// Contrast, dimensionless, inclusive `[-1000, 1000]`.
    pub contrast: f64,
    /// Brightness offset, dimensionless, inclusive `[-1, 1]`.
    pub brightness: f64,
    /// Saturation, dimensionless, inclusive `[0, 3]`.
    pub saturation: f64,
    /// Gamma, dimensionless, inclusive `[0.1, 10]`.
    pub gamma: f64,
    /// Red gamma, dimensionless, inclusive `[0.1, 10]`.
    pub gamma_r: f64,
    /// Green gamma, dimensionless, inclusive `[0.1, 10]`.
    pub gamma_g: f64,
    /// Blue gamma, dimensionless, inclusive `[0.1, 10]`.
    pub gamma_b: f64,
    /// Gamma weight, dimensionless, inclusive `[0, 1]`.
    pub gamma_weight: f64,
}

/// Raw `unsharp` inputs, validated by [`UnsharpParams::new`]. Matrix sizes are integer taps.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnsharpValues {
    /// Luma matrix width in taps, odd `3..=23`.
    pub luma_msize_x: u32,
    /// Luma matrix height in taps, odd `3..=23`.
    pub luma_msize_y: u32,
    /// Luma strength, dimensionless, inclusive `[-2, 5]`.
    pub luma_amount: f64,
    /// Chroma matrix width in taps, odd `3..=23`.
    pub chroma_msize_x: u32,
    /// Chroma matrix height in taps, odd `3..=23`.
    pub chroma_msize_y: u32,
    /// Chroma strength, dimensionless, inclusive `[-2, 5]`.
    pub chroma_amount: f64,
    /// Alpha matrix width in taps, odd `3..=23`.
    pub alpha_msize_x: u32,
    /// Alpha matrix height in taps, odd `3..=23`.
    pub alpha_msize_y: u32,
    /// Alpha strength, dimensionless, inclusive `[-2, 5]`.
    pub alpha_amount: f64,
}

/// Raw `hqdn3d` inputs, validated by [`Hqdn3dParams::new`]. Strengths are unitless.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hqdn3dValues {
    /// Luma spatial strength, inclusive `[0, 252]`.
    pub luma_spatial: f64,
    /// Chroma spatial strength, inclusive `[0, 252]`.
    pub chroma_spatial: f64,
    /// Luma temporal strength, inclusive `[0, 252]`.
    pub luma_tmp: f64,
    /// Chroma temporal strength, inclusive `[0, 252]`.
    pub chroma_tmp: f64,
}

// ---------------------------------------------------------------------------
// Format
// ---------------------------------------------------------------------------

/// SDR color matrix (mpv `format=colormatrix`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum SdrMatrix {
    /// Automatic selection.
    #[serde(rename = "auto")]
    Auto,
    /// BT.601.
    #[serde(rename = "bt601")]
    Bt601,
    /// BT.709.
    #[serde(rename = "bt709")]
    Bt709,
    /// SMPTE 240M.
    #[serde(rename = "smpte240m")]
    Smpte240m,
    /// RGB.
    #[serde(rename = "rgb")]
    Rgb,
}

/// Color levels / range (mpv `format=colorlevels`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ColorLevels {
    /// Automatic selection.
    #[serde(rename = "auto")]
    Auto,
    /// Limited range.
    #[serde(rename = "limited")]
    Limited,
    /// Full range.
    #[serde(rename = "full")]
    Full,
}

/// SDR transfer gamma (mpv `format=gamma`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum SdrGamma {
    /// Automatic selection.
    #[serde(rename = "auto")]
    Auto,
    /// BT.1886.
    #[serde(rename = "bt1886")]
    Bt1886,
    /// sRGB.
    #[serde(rename = "srgb")]
    Srgb,
    /// Linear.
    #[serde(rename = "linear")]
    Linear,
    /// Gamma 1.8.
    #[serde(rename = "gamma18")]
    Gamma18,
    /// Gamma 2.0.
    #[serde(rename = "gamma20")]
    Gamma20,
    /// Gamma 2.2.
    #[serde(rename = "gamma22")]
    Gamma22,
    /// Gamma 2.4.
    #[serde(rename = "gamma24")]
    Gamma24,
    /// Gamma 2.6.
    #[serde(rename = "gamma26")]
    Gamma26,
    /// Gamma 2.8.
    #[serde(rename = "gamma28")]
    Gamma28,
}

/// Validated SDR `format` parameters: matrix, levels and gamma only (no primaries, no
/// pixel-format/scale controls).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct FormatParams {
    matrix: SdrMatrix,
    levels: ColorLevels,
    gamma: SdrGamma,
}

impl FormatParams {
    /// Build SDR format parameters. All enum values are inherently valid.
    pub fn new(matrix: SdrMatrix, levels: ColorLevels, gamma: SdrGamma) -> Self {
        Self {
            matrix,
            levels,
            gamma,
        }
    }

    /// Color matrix.
    pub fn matrix(&self) -> SdrMatrix {
        self.matrix
    }

    /// Color levels.
    pub fn levels(&self) -> ColorLevels {
        self.levels
    }

    /// Transfer gamma.
    pub fn gamma(&self) -> SdrGamma {
        self.gamma
    }
}

// ---------------------------------------------------------------------------
// Eq
// ---------------------------------------------------------------------------

/// Validated `eq` parameters. Effective intervals from the FFmpeg n8.1.2 `vf_eq.c`
/// evaluation clip (<https://github.com/FFmpeg/FFmpeg/blob/n8.1.2/libavfilter/vf_eq.c>);
/// out-of-interval scalars are rejected rather than silently clamped. No expressions.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct EqParams {
    contrast: f64,
    brightness: f64,
    saturation: f64,
    gamma: f64,
    gamma_r: f64,
    gamma_g: f64,
    gamma_b: f64,
    gamma_weight: f64,
}

impl EqParams {
    /// Validate and build `eq` parameters. Rejects non-finite and out-of-interval values.
    pub fn new(values: EqValues) -> Result<Self, FilterValidationError> {
        let kind = FilterKind::Eq;
        Ok(Self {
            contrast: bounded(kind, "contrast", values.contrast, -1000.0, 1000.0)?,
            brightness: bounded(kind, "brightness", values.brightness, -1.0, 1.0)?,
            saturation: bounded(kind, "saturation", values.saturation, 0.0, 3.0)?,
            gamma: bounded(kind, "gamma", values.gamma, 0.1, 10.0)?,
            gamma_r: bounded(kind, "gamma_r", values.gamma_r, 0.1, 10.0)?,
            gamma_g: bounded(kind, "gamma_g", values.gamma_g, 0.1, 10.0)?,
            gamma_b: bounded(kind, "gamma_b", values.gamma_b, 0.1, 10.0)?,
            gamma_weight: bounded(kind, "gamma_weight", values.gamma_weight, 0.0, 1.0)?,
        })
    }

    /// Contrast, `[-1000, 1000]`.
    pub fn contrast(&self) -> f64 {
        self.contrast
    }

    /// Brightness offset, `[-1, 1]`.
    pub fn brightness(&self) -> f64 {
        self.brightness
    }

    /// Saturation, `[0, 3]`.
    pub fn saturation(&self) -> f64 {
        self.saturation
    }

    /// Gamma, `[0.1, 10]`.
    pub fn gamma(&self) -> f64 {
        self.gamma
    }

    /// Red gamma, `[0.1, 10]`.
    pub fn gamma_r(&self) -> f64 {
        self.gamma_r
    }

    /// Green gamma, `[0.1, 10]`.
    pub fn gamma_g(&self) -> f64 {
        self.gamma_g
    }

    /// Blue gamma, `[0.1, 10]`.
    pub fn gamma_b(&self) -> f64 {
        self.gamma_b
    }

    /// Gamma weight, `[0, 1]`.
    pub fn gamma_weight(&self) -> f64 {
        self.gamma_weight
    }
}

// ---------------------------------------------------------------------------
// Unsharp
// ---------------------------------------------------------------------------

/// Validated `unsharp` parameters. Matrix dimensions are odd tap counts in `3..=23`
/// (rectangular matrices allowed); amounts are dimensionless strengths in `[-2, 5]`
/// (negative blurs, positive sharpens). Alpha fields are exposed as filter capability and
/// make no promise of effect on formats without alpha.
/// Source: <https://github.com/FFmpeg/FFmpeg/blob/n8.1.2/libavfilter/vf_unsharp.c>.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct UnsharpParams {
    luma_msize_x: u8,
    luma_msize_y: u8,
    luma_amount: f64,
    chroma_msize_x: u8,
    chroma_msize_y: u8,
    chroma_amount: f64,
    alpha_msize_x: u8,
    alpha_msize_y: u8,
    alpha_amount: f64,
}

impl UnsharpParams {
    /// Validate and build `unsharp` parameters.
    pub fn new(values: UnsharpValues) -> Result<Self, FilterValidationError> {
        let kind = FilterKind::Unsharp;
        Ok(Self {
            luma_msize_x: odd_matrix_size(kind, "luma_msize_x", values.luma_msize_x)?,
            luma_msize_y: odd_matrix_size(kind, "luma_msize_y", values.luma_msize_y)?,
            luma_amount: bounded(kind, "luma_amount", values.luma_amount, -2.0, 5.0)?,
            chroma_msize_x: odd_matrix_size(kind, "chroma_msize_x", values.chroma_msize_x)?,
            chroma_msize_y: odd_matrix_size(kind, "chroma_msize_y", values.chroma_msize_y)?,
            chroma_amount: bounded(kind, "chroma_amount", values.chroma_amount, -2.0, 5.0)?,
            alpha_msize_x: odd_matrix_size(kind, "alpha_msize_x", values.alpha_msize_x)?,
            alpha_msize_y: odd_matrix_size(kind, "alpha_msize_y", values.alpha_msize_y)?,
            alpha_amount: bounded(kind, "alpha_amount", values.alpha_amount, -2.0, 5.0)?,
        })
    }

    /// Luma matrix width, odd `3..=23`.
    pub fn luma_msize_x(&self) -> u8 {
        self.luma_msize_x
    }

    /// Luma matrix height, odd `3..=23`.
    pub fn luma_msize_y(&self) -> u8 {
        self.luma_msize_y
    }

    /// Luma strength, `[-2, 5]`.
    pub fn luma_amount(&self) -> f64 {
        self.luma_amount
    }

    /// Chroma matrix width, odd `3..=23`.
    pub fn chroma_msize_x(&self) -> u8 {
        self.chroma_msize_x
    }

    /// Chroma matrix height, odd `3..=23`.
    pub fn chroma_msize_y(&self) -> u8 {
        self.chroma_msize_y
    }

    /// Chroma strength, `[-2, 5]`.
    pub fn chroma_amount(&self) -> f64 {
        self.chroma_amount
    }

    /// Alpha matrix width, odd `3..=23`.
    pub fn alpha_msize_x(&self) -> u8 {
        self.alpha_msize_x
    }

    /// Alpha matrix height, odd `3..=23`.
    pub fn alpha_msize_y(&self) -> u8 {
        self.alpha_msize_y
    }

    /// Alpha strength, `[-2, 5]`.
    pub fn alpha_amount(&self) -> f64 {
        self.alpha_amount
    }
}

// ---------------------------------------------------------------------------
// Hqdn3d
// ---------------------------------------------------------------------------

/// Validated `hqdn3d` parameters: four unitless strength scalars in inclusive `[0, 252]`.
///
/// `252` is the effective saturation of the coefficient function
/// (`FFMIN(dist25, 252.0)` in `vf_hqdn3d.c`), an approved product cap, not an advertised
/// AVOption maximum. Source:
/// <https://github.com/FFmpeg/FFmpeg/blob/n8.1.2/libavfilter/vf_hqdn3d.c>.
///
/// `0.0` is FFmpeg's sentinel that derives a relational default (all-zero yields
/// `4/3/6/4.5`), not a bypass/disable. Literal values are preserved; the domain neither
/// resolves nor clamps them.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Hqdn3dParams {
    luma_spatial: f64,
    chroma_spatial: f64,
    luma_tmp: f64,
    chroma_tmp: f64,
}

impl Hqdn3dParams {
    /// Validate and build `hqdn3d` parameters. Rejects non-finite and out-of-`[0, 252]`
    /// values.
    pub fn new(values: Hqdn3dValues) -> Result<Self, FilterValidationError> {
        let kind = FilterKind::Hqdn3d;
        Ok(Self {
            luma_spatial: bounded(kind, "luma_spatial", values.luma_spatial, 0.0, 252.0)?,
            chroma_spatial: bounded(kind, "chroma_spatial", values.chroma_spatial, 0.0, 252.0)?,
            luma_tmp: bounded(kind, "luma_tmp", values.luma_tmp, 0.0, 252.0)?,
            chroma_tmp: bounded(kind, "chroma_tmp", values.chroma_tmp, 0.0, 252.0)?,
        })
    }

    /// Luma spatial strength, `[0, 252]`.
    pub fn luma_spatial(&self) -> f64 {
        self.luma_spatial
    }

    /// Chroma spatial strength, `[0, 252]`.
    pub fn chroma_spatial(&self) -> f64 {
        self.chroma_spatial
    }

    /// Luma temporal strength, `[0, 252]`.
    pub fn luma_tmp(&self) -> f64 {
        self.luma_tmp
    }

    /// Chroma temporal strength, `[0, 252]`.
    pub fn chroma_tmp(&self) -> f64 {
        self.chroma_tmp
    }
}

// ---------------------------------------------------------------------------
// Bwdif
// ---------------------------------------------------------------------------

/// `bwdif` mode. Source: <https://github.com/FFmpeg/FFmpeg/blob/n8.1.2/libavfilter/vf_bwdif.c>.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum BwdifMode {
    /// Deinterlace one input frame to one output frame.
    #[serde(rename = "send_frame")]
    SendFrame,
    /// Deinterlace one field to one frame.
    #[serde(rename = "send_field")]
    SendField,
}

/// Field parity for `bwdif`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FieldParity {
    /// Automatic parity detection.
    #[serde(rename = "auto")]
    Auto,
    /// Top field first.
    #[serde(rename = "top_field_first")]
    TopFieldFirst,
    /// Bottom field first.
    #[serde(rename = "bottom_field_first")]
    BottomFieldFirst,
}

/// Which frames `bwdif` deinterlaces.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum DeinterlaceSelection {
    /// Deinterlace all frames.
    #[serde(rename = "all")]
    All,
    /// Deinterlace only frames flagged interlaced.
    #[serde(rename = "interlaced")]
    Interlaced,
}

/// Validated `bwdif` parameters: discrete enums only, no numeric escape hatch.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct BwdifParams {
    mode: BwdifMode,
    parity: FieldParity,
    deint: DeinterlaceSelection,
}

impl BwdifParams {
    /// Build `bwdif` parameters. All enum values are inherently valid.
    pub fn new(mode: BwdifMode, parity: FieldParity, deint: DeinterlaceSelection) -> Self {
        Self {
            mode,
            parity,
            deint,
        }
    }

    /// Deinterlacing mode.
    pub fn mode(&self) -> BwdifMode {
        self.mode
    }

    /// Field parity.
    pub fn parity(&self) -> FieldParity {
        self.parity
    }

    /// Frame selection.
    pub fn deint(&self) -> DeinterlaceSelection {
        self.deint
    }
}

// ---------------------------------------------------------------------------
// Filter
// ---------------------------------------------------------------------------

/// A validated product filter. The variant determines the kind, so kind and payload can
/// never disagree; there is no raw/options/expression variant.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", content = "parameters")]
pub enum Filter {
    /// mpv-native SDR `format`.
    #[serde(rename = "format")]
    Format(FormatParams),
    /// FFmpeg `eq`.
    #[serde(rename = "eq")]
    Eq(EqParams),
    /// FFmpeg `unsharp`.
    #[serde(rename = "unsharp")]
    Unsharp(UnsharpParams),
    /// FFmpeg `hqdn3d`.
    #[serde(rename = "hqdn3d")]
    Hqdn3d(Hqdn3dParams),
    /// FFmpeg `bwdif`.
    #[serde(rename = "bwdif")]
    Bwdif(BwdifParams),
}

impl Filter {
    /// The kind of this filter, from the payload variant.
    pub fn kind(&self) -> FilterKind {
        match self {
            Filter::Format(_) => FilterKind::Format,
            Filter::Eq(_) => FilterKind::Eq,
            Filter::Unsharp(_) => FilterKind::Unsharp,
            Filter::Hqdn3d(_) => FilterKind::Hqdn3d,
            Filter::Bwdif(_) => FilterKind::Bwdif,
        }
    }
}

// ---------------------------------------------------------------------------
// Entry and chain
// ---------------------------------------------------------------------------

/// One ordered chain entry: an inert display label, a validated filter and an enabled flag.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FilterEntry {
    label: String,
    filter: Filter,
    enabled: bool,
}

impl FilterEntry {
    /// Build an entry. The label is inert display text: no character, length or emptiness
    /// restriction is imposed, and it is never interpreted as syntax.
    pub fn new(label: String, filter: Filter, enabled: bool) -> Self {
        Self {
            label,
            filter,
            enabled,
        }
    }

    /// Display label (inert).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The validated filter payload.
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// The filter kind.
    pub fn kind(&self) -> FilterKind {
        self.filter.kind()
    }

    /// Whether this entry participates in emission.
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

/// An ordered chain of filter entries with exactly-unique labels (across enabled and
/// disabled entries). Editing methods prevalidate before mutating, so a rejected edit
/// leaves the chain byte-for-byte equal to its prior value.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FilterChain {
    entries: Vec<FilterEntry>,
}

impl FilterChain {
    /// Build a chain, rejecting duplicate labels by exact `String` equality.
    pub fn new(entries: Vec<FilterEntry>) -> Result<Self, FilterValidationError> {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            if let Some(&first) = seen.get(entry.label.as_str()) {
                return Err(FilterValidationError::DuplicateLabel {
                    label: entry.label.clone(),
                    first,
                    duplicate: index,
                });
            }
            seen.insert(entry.label.as_str(), index);
        }
        Ok(Self { entries })
    }

    /// The entries in order.
    pub fn entries(&self) -> &[FilterEntry] {
        &self.entries
    }

    /// Insert `entry` at `index` (`0..=len`). Rejects a duplicate label.
    pub fn insert(
        &mut self,
        index: usize,
        entry: FilterEntry,
    ) -> Result<(), FilterValidationError> {
        let len = self.entries.len();
        if index > len {
            return Err(FilterValidationError::InvalidIndex { index, len });
        }
        if let Some(first) = self.entries.iter().position(|e| e.label == entry.label) {
            return Err(FilterValidationError::DuplicateLabel {
                label: entry.label,
                first,
                duplicate: index,
            });
        }
        self.entries.insert(index, entry);
        Ok(())
    }

    /// Remove and return the entry at `index` (`0..len`).
    pub fn remove(&mut self, index: usize) -> Result<FilterEntry, FilterValidationError> {
        if index >= self.entries.len() {
            return Err(FilterValidationError::InvalidIndex {
                index,
                len: self.entries.len(),
            });
        }
        Ok(self.entries.remove(index))
    }

    /// Move the entry at `from` so that its final index is `to`. Both must be valid entry
    /// indices; a same-index move is an unchanged success.
    pub fn move_entry(&mut self, from: usize, to: usize) -> Result<(), FilterValidationError> {
        let len = self.entries.len();
        if from >= len {
            return Err(FilterValidationError::InvalidIndex { index: from, len });
        }
        if to >= len {
            return Err(FilterValidationError::InvalidIndex { index: to, len });
        }
        if from == to {
            return Ok(());
        }
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        Ok(())
    }

    /// Replace the entry at `index` (`0..len`). The replaced entry's own label is excluded
    /// from the duplicate check, so a same-label replacement succeeds.
    pub fn replace(
        &mut self,
        index: usize,
        entry: FilterEntry,
    ) -> Result<(), FilterValidationError> {
        if index >= self.entries.len() {
            return Err(FilterValidationError::InvalidIndex {
                index,
                len: self.entries.len(),
            });
        }
        if let Some(first) = self.conflict(&entry.label, index) {
            return Err(FilterValidationError::DuplicateLabel {
                label: entry.label,
                first,
                duplicate: index,
            });
        }
        self.entries[index] = entry;
        Ok(())
    }

    /// Rename the entry at `index` (`0..len`). The entry's current label is excluded from
    /// the duplicate check, so a no-op rename succeeds.
    pub fn rename(&mut self, index: usize, label: String) -> Result<(), FilterValidationError> {
        if index >= self.entries.len() {
            return Err(FilterValidationError::InvalidIndex {
                index,
                len: self.entries.len(),
            });
        }
        if let Some(first) = self.conflict(&label, index) {
            return Err(FilterValidationError::DuplicateLabel {
                label,
                first,
                duplicate: index,
            });
        }
        self.entries[index].label = label;
        Ok(())
    }

    /// Replace the filter payload at `index` (`0..len`); the label and enabled flag are
    /// preserved.
    pub fn set_filter(
        &mut self,
        index: usize,
        filter: Filter,
    ) -> Result<(), FilterValidationError> {
        if index >= self.entries.len() {
            return Err(FilterValidationError::InvalidIndex {
                index,
                len: self.entries.len(),
            });
        }
        self.entries[index].filter = filter;
        Ok(())
    }

    /// Set the enabled flag at `index` (`0..len`).
    pub fn set_enabled(
        &mut self,
        index: usize,
        enabled: bool,
    ) -> Result<(), FilterValidationError> {
        if index >= self.entries.len() {
            return Err(FilterValidationError::InvalidIndex {
                index,
                len: self.entries.len(),
            });
        }
        self.entries[index].enabled = enabled;
        Ok(())
    }

    /// First index other than `skip` already using `label`, if any.
    fn conflict(&self, label: &str, skip: usize) -> Option<usize> {
        self.entries
            .iter()
            .enumerate()
            .find(|(i, e)| *i != skip && e.label == label)
            .map(|(i, _)| i)
    }
}

// ---------------------------------------------------------------------------
// Strict deserialization
// ---------------------------------------------------------------------------

/// Require a JSON object before delegating named-field checks to Serde (a derived struct
/// deserializer would also accept positional sequences).
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

macro_rules! impl_params_deserialize {
    ($params:ty, $values:ident, { $($field:ident),* $(,)? }) => {
        impl<'de> Deserialize<'de> for $params {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Fields {
                    $($field: f64),*
                }
                let fields: Fields = deserialize_object(deserializer)?;
                <$params>::new($values { $($field: fields.$field),* }).map_err(de::Error::custom)
            }
        }
    };
}

impl_params_deserialize!(EqParams, EqValues, {
    contrast,
    brightness,
    saturation,
    gamma,
    gamma_r,
    gamma_g,
    gamma_b,
    gamma_weight,
});

impl<'de> Deserialize<'de> for UnsharpParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            luma_msize_x: u32,
            luma_msize_y: u32,
            luma_amount: f64,
            chroma_msize_x: u32,
            chroma_msize_y: u32,
            chroma_amount: f64,
            alpha_msize_x: u32,
            alpha_msize_y: u32,
            alpha_amount: f64,
        }
        let fields: Fields = deserialize_object(deserializer)?;
        UnsharpParams::new(UnsharpValues {
            luma_msize_x: fields.luma_msize_x,
            luma_msize_y: fields.luma_msize_y,
            luma_amount: fields.luma_amount,
            chroma_msize_x: fields.chroma_msize_x,
            chroma_msize_y: fields.chroma_msize_y,
            chroma_amount: fields.chroma_amount,
            alpha_msize_x: fields.alpha_msize_x,
            alpha_msize_y: fields.alpha_msize_y,
            alpha_amount: fields.alpha_amount,
        })
        .map_err(de::Error::custom)
    }
}

impl_params_deserialize!(Hqdn3dParams, Hqdn3dValues, {
    luma_spatial,
    chroma_spatial,
    luma_tmp,
    chroma_tmp,
});

impl<'de> Deserialize<'de> for FormatParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            matrix: SdrMatrix,
            levels: ColorLevels,
            gamma: SdrGamma,
        }
        let fields: Fields = deserialize_object(deserializer)?;
        Ok(FormatParams::new(
            fields.matrix,
            fields.levels,
            fields.gamma,
        ))
    }
}

impl<'de> Deserialize<'de> for BwdifParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            mode: BwdifMode,
            parity: FieldParity,
            deint: DeinterlaceSelection,
        }
        let fields: Fields = deserialize_object(deserializer)?;
        Ok(BwdifParams::new(fields.mode, fields.parity, fields.deint))
    }
}

/// Strict typed envelope. Serde's adjacent-tag buffering preserves map entries when
/// `parameters` precedes `kind`, so parameter deserializers still reject duplicate fields.
#[derive(Deserialize)]
#[serde(tag = "kind", content = "parameters", deny_unknown_fields)]
enum FilterEnvelope {
    #[serde(rename = "format")]
    Format(FormatParams),
    #[serde(rename = "eq")]
    Eq(EqParams),
    #[serde(rename = "unsharp")]
    Unsharp(UnsharpParams),
    #[serde(rename = "hqdn3d")]
    Hqdn3d(Hqdn3dParams),
    #[serde(rename = "bwdif")]
    Bwdif(BwdifParams),
}

impl<'de> Deserialize<'de> for Filter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let envelope = deserialize_object(deserializer)?;
        Ok(match envelope {
            FilterEnvelope::Format(params) => Filter::Format(params),
            FilterEnvelope::Eq(params) => Filter::Eq(params),
            FilterEnvelope::Unsharp(params) => Filter::Unsharp(params),
            FilterEnvelope::Hqdn3d(params) => Filter::Hqdn3d(params),
            FilterEnvelope::Bwdif(params) => Filter::Bwdif(params),
        })
    }
}

impl<'de> Deserialize<'de> for FilterEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            label: String,
            filter: Filter,
            enabled: bool,
        }
        let fields: Fields = deserialize_object(deserializer)?;
        Ok(FilterEntry {
            label: fields.label,
            filter: fields.filter,
            enabled: fields.enabled,
        })
    }
}

impl<'de> Deserialize<'de> for FilterChain {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            entries: Vec<FilterEntry>,
        }
        let fields: Fields = deserialize_object(deserializer)?;
        FilterChain::new(fields.entries).map_err(de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base_eq() -> EqValues {
        EqValues {
            contrast: 1.0,
            brightness: 0.0,
            saturation: 1.0,
            gamma: 1.0,
            gamma_r: 1.0,
            gamma_g: 1.0,
            gamma_b: 1.0,
            gamma_weight: 1.0,
        }
    }

    fn eq_filter() -> Filter {
        Filter::Eq(EqParams::new(base_eq()).unwrap())
    }

    fn entry(label: &str, filter: Filter, enabled: bool) -> FilterEntry {
        FilterEntry::new(label.to_string(), filter, enabled)
    }

    fn format_filter() -> Filter {
        Filter::Format(FormatParams::new(
            SdrMatrix::Bt709,
            ColorLevels::Limited,
            SdrGamma::Bt1886,
        ))
    }

    fn hqdn3d_filter() -> Filter {
        Filter::Hqdn3d(
            Hqdn3dParams::new(Hqdn3dValues {
                luma_spatial: 3.0,
                chroma_spatial: 0.0,
                luma_tmp: 6.0,
                chroma_tmp: 4.5,
            })
            .unwrap(),
        )
    }

    fn assert_filter_roundtrip(filter: Filter) {
        let chain = FilterChain::new(vec![
            entry("first", filter, false),
            entry("second", format_filter(), true),
        ])
        .unwrap();
        let json = serde_json::to_string(&chain).unwrap();
        assert_eq!(serde_json::from_str::<FilterChain>(&json).unwrap(), chain);
    }

    fn assert_invalid_parameter_decode(kind: FilterKind, parameters: serde_json::Value) {
        let filter = serde_json::json!({"kind": kind, "parameters": parameters});
        assert!(serde_json::from_value::<Filter>(filter.clone()).is_err());
        let chain = serde_json::json!({
            "entries": [{"label": "invalid", "enabled": false, "filter": filter}]
        });
        assert!(serde_json::from_value::<FilterChain>(chain).is_err());
    }

    macro_rules! check_numeric_field {
        ($base:expr, $params:ty, $variant:ident, $field:ident, $min:expr, $max:expr) => {{
            for value in [$min, $max] {
                let mut values = $base;
                values.$field = value;
                let params = <$params>::new(values).unwrap();
                assert_eq!(params.$field(), value);
                assert_filter_roundtrip(Filter::$variant(params));
            }
            for value in [$min - 0.0001, $max + 0.0001] {
                let mut values = $base;
                values.$field = value;
                assert!(matches!(
                    <$params>::new(values),
                    Err(FilterValidationError::OutOfBounds {
                        filter: FilterKind::$variant,
                        parameter: stringify!($field),
                        ..
                    })
                ));
                let mut parameters = serde_json::to_value(<$params>::new($base).unwrap()).unwrap();
                parameters[stringify!($field)] = serde_json::json!(value);
                assert!(serde_json::from_value::<$params>(parameters.clone()).is_err());
                assert_invalid_parameter_decode(FilterKind::$variant, parameters);
            }
            for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                let mut values = $base;
                values.$field = value;
                assert!(matches!(
                    <$params>::new(values),
                    Err(FilterValidationError::NonFinite {
                        filter: FilterKind::$variant,
                        parameter: stringify!($field),
                        ..
                    })
                ));
            }
        }};
    }

    // --- numeric bounds -----------------------------------------------------

    #[test]
    fn every_eq_field_enforces_its_numeric_contract() {
        check_numeric_field!(base_eq(), EqParams, Eq, contrast, -1000.0, 1000.0);
        check_numeric_field!(base_eq(), EqParams, Eq, brightness, -1.0, 1.0);
        check_numeric_field!(base_eq(), EqParams, Eq, saturation, 0.0, 3.0);
        check_numeric_field!(base_eq(), EqParams, Eq, gamma, 0.1, 10.0);
        check_numeric_field!(base_eq(), EqParams, Eq, gamma_r, 0.1, 10.0);
        check_numeric_field!(base_eq(), EqParams, Eq, gamma_g, 0.1, 10.0);
        check_numeric_field!(base_eq(), EqParams, Eq, gamma_b, 0.1, 10.0);
        check_numeric_field!(base_eq(), EqParams, Eq, gamma_weight, 0.0, 1.0);
    }

    #[test]
    fn signed_zero_normalized_to_positive() {
        let mut v = base_eq();
        v.brightness = -0.0;
        let params = EqParams::new(v).unwrap();
        assert!(params.brightness().is_sign_positive());
        assert_eq!(params.brightness().to_bits(), 0.0f64.to_bits());
    }

    fn base_unsharp() -> UnsharpValues {
        UnsharpValues {
            luma_msize_x: 3,
            luma_msize_y: 5,
            luma_amount: 1.0,
            chroma_msize_x: 7,
            chroma_msize_y: 9,
            chroma_amount: 0.0,
            alpha_msize_x: 11,
            alpha_msize_y: 13,
            alpha_amount: 0.0,
        }
    }

    #[test]
    fn every_unsharp_dimension_requires_odd_taps_in_range() {
        macro_rules! check_dimension {
            ($field:ident) => {
                for value in (3..=23).step_by(2) {
                    let mut values = base_unsharp();
                    values.$field = value;
                    let params = UnsharpParams::new(values).unwrap();
                    assert_eq!(u32::from(params.$field()), value);
                    assert_filter_roundtrip(Filter::Unsharp(params));
                }
                for value in [0, 1, 2, 4, 22, 24, 25, u32::MAX] {
                    let mut values = base_unsharp();
                    values.$field = value;
                    assert!(matches!(
                        UnsharpParams::new(values),
                        Err(FilterValidationError::InvalidMatrixSize {
                            filter: FilterKind::Unsharp,
                            parameter: stringify!($field),
                            ..
                        })
                    ));
                    let mut parameters =
                        serde_json::to_value(UnsharpParams::new(base_unsharp()).unwrap()).unwrap();
                    parameters[stringify!($field)] = serde_json::json!(value);
                    assert!(serde_json::from_value::<UnsharpParams>(parameters.clone()).is_err());
                    assert_invalid_parameter_decode(FilterKind::Unsharp, parameters);
                }
            };
        }
        check_dimension!(luma_msize_x);
        check_dimension!(luma_msize_y);
        check_dimension!(chroma_msize_x);
        check_dimension!(chroma_msize_y);
        check_dimension!(alpha_msize_x);
        check_dimension!(alpha_msize_y);
    }

    #[test]
    fn every_unsharp_amount_enforces_its_numeric_contract() {
        check_numeric_field!(
            base_unsharp(),
            UnsharpParams,
            Unsharp,
            luma_amount,
            -2.0,
            5.0
        );
        check_numeric_field!(
            base_unsharp(),
            UnsharpParams,
            Unsharp,
            chroma_amount,
            -2.0,
            5.0
        );
        check_numeric_field!(
            base_unsharp(),
            UnsharpParams,
            Unsharp,
            alpha_amount,
            -2.0,
            5.0
        );
    }

    fn base_hqdn3d() -> Hqdn3dValues {
        Hqdn3dValues {
            luma_spatial: 4.0,
            chroma_spatial: 3.0,
            luma_tmp: 6.0,
            chroma_tmp: 4.5,
        }
    }

    #[test]
    fn every_hqdn3d_field_enforces_its_numeric_contract() {
        check_numeric_field!(
            base_hqdn3d(),
            Hqdn3dParams,
            Hqdn3d,
            luma_spatial,
            0.0,
            252.0
        );
        check_numeric_field!(
            base_hqdn3d(),
            Hqdn3dParams,
            Hqdn3d,
            chroma_spatial,
            0.0,
            252.0
        );
        check_numeric_field!(base_hqdn3d(), Hqdn3dParams, Hqdn3d, luma_tmp, 0.0, 252.0);
        check_numeric_field!(base_hqdn3d(), Hqdn3dParams, Hqdn3d, chroma_tmp, 0.0, 252.0);
    }

    #[test]
    fn hqdn3d_zero_sentinels_remain_literal() {
        let params = Hqdn3dParams::new(Hqdn3dValues {
            luma_spatial: 0.0,
            chroma_spatial: 0.0,
            luma_tmp: 0.0,
            chroma_tmp: 0.0,
        })
        .unwrap();
        assert_eq!(params.luma_spatial(), 0.0);
        assert_eq!(params.chroma_spatial(), 0.0);
        assert_eq!(params.luma_tmp(), 0.0);
        assert_eq!(params.chroma_tmp(), 0.0);
        assert_filter_roundtrip(Filter::Hqdn3d(params));
    }

    // --- enums --------------------------------------------------------------

    #[test]
    fn every_enum_variant_roundtrips_through_json() {
        for matrix in [
            SdrMatrix::Auto,
            SdrMatrix::Bt601,
            SdrMatrix::Bt709,
            SdrMatrix::Smpte240m,
            SdrMatrix::Rgb,
        ] {
            assert_filter_roundtrip(Filter::Format(FormatParams::new(
                matrix,
                ColorLevels::Limited,
                SdrGamma::Bt1886,
            )));
        }
        for levels in [ColorLevels::Auto, ColorLevels::Limited, ColorLevels::Full] {
            assert_filter_roundtrip(Filter::Format(FormatParams::new(
                SdrMatrix::Smpte240m,
                levels,
                SdrGamma::Gamma28,
            )));
        }
        for gamma in [
            SdrGamma::Auto,
            SdrGamma::Bt1886,
            SdrGamma::Srgb,
            SdrGamma::Linear,
            SdrGamma::Gamma18,
            SdrGamma::Gamma20,
            SdrGamma::Gamma22,
            SdrGamma::Gamma24,
            SdrGamma::Gamma26,
            SdrGamma::Gamma28,
        ] {
            assert_filter_roundtrip(Filter::Format(FormatParams::new(
                SdrMatrix::Rgb,
                ColorLevels::Full,
                gamma,
            )));
        }
        for mode in [BwdifMode::SendFrame, BwdifMode::SendField] {
            assert_filter_roundtrip(Filter::Bwdif(BwdifParams::new(
                mode,
                FieldParity::BottomFieldFirst,
                DeinterlaceSelection::Interlaced,
            )));
        }
        for parity in [
            FieldParity::Auto,
            FieldParity::TopFieldFirst,
            FieldParity::BottomFieldFirst,
        ] {
            assert_filter_roundtrip(Filter::Bwdif(BwdifParams::new(
                BwdifMode::SendField,
                parity,
                DeinterlaceSelection::All,
            )));
        }
        for deint in [DeinterlaceSelection::All, DeinterlaceSelection::Interlaced] {
            assert_filter_roundtrip(Filter::Bwdif(BwdifParams::new(
                BwdifMode::SendFrame,
                FieldParity::TopFieldFirst,
                deint,
            )));
        }
        for kind in FilterKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<FilterKind>(&json).unwrap(), kind);
        }
    }

    // --- chain validation ---------------------------------------------------

    #[test]
    fn empty_chain_valid() {
        assert_eq!(FilterChain::new(vec![]).unwrap(), FilterChain::default());
        assert!(
            serde_json::from_str::<FilterChain>(r#"{"entries":[]}"#)
                .unwrap()
                .entries()
                .is_empty()
        );
    }

    #[test]
    fn duplicate_labels_rejected_across_enabled_states() {
        let enabled = entry("dup", eq_filter(), true);
        let disabled = entry("dup", format_filter(), false);
        assert!(matches!(
            FilterChain::new(vec![enabled.clone(), disabled]),
            Err(FilterValidationError::DuplicateLabel {
                first: 0,
                duplicate: 1,
                ..
            })
        ));
        assert!(matches!(
            FilterChain::new(vec![enabled.clone(), enabled]),
            Err(FilterValidationError::DuplicateLabel { .. })
        ));
    }

    #[test]
    fn labels_compared_by_exact_equality_only() {
        let chain = FilterChain::new(vec![
            entry("A", eq_filter(), true),
            entry("a", eq_filter(), true),
            entry(" A ", eq_filter(), true),
        ]);
        assert!(chain.is_ok());
    }

    #[test]
    fn repeated_kind_with_distinct_labels_allowed() {
        let chain = FilterChain::new(vec![
            entry("first", eq_filter(), true),
            entry("second", eq_filter(), false),
        ]);
        assert!(chain.is_ok());
    }

    // --- atomic edits -------------------------------------------------------

    #[test]
    fn invalid_edits_leave_chain_unchanged() {
        let original = FilterChain::new(vec![
            entry("a", eq_filter(), true),
            entry("b", format_filter(), false),
        ])
        .unwrap();

        let mut chain = original.clone();
        assert!(chain.insert(3, entry("x", eq_filter(), true)).is_err());
        assert_eq!(chain, original);

        let mut chain = original.clone();
        assert!(chain.insert(0, entry("b", eq_filter(), true)).is_err());
        assert_eq!(chain, original);

        let mut chain = original.clone();
        assert!(chain.rename(0, "b".to_string()).is_err());
        assert_eq!(chain, original);

        let mut chain = original.clone();
        assert!(chain.replace(0, entry("b", eq_filter(), true)).is_err());
        assert_eq!(chain, original);

        let mut chain = original.clone();
        assert!(chain.move_entry(0, 2).is_err());
        assert_eq!(chain, original);

        let mut chain = original.clone();
        assert!(chain.remove(2).is_err());
        assert_eq!(chain, original);

        let mut chain = original.clone();
        assert!(chain.move_entry(2, 0).is_err());
        assert_eq!(chain, original);

        let mut chain = original.clone();
        assert!(chain.rename(2, "x".to_string()).is_err());
        assert_eq!(chain, original);

        let mut chain = original.clone();
        assert!(chain.replace(2, entry("x", eq_filter(), true)).is_err());
        assert_eq!(chain, original);

        let mut chain = original.clone();
        assert!(chain.set_filter(2, hqdn3d_filter()).is_err());
        assert_eq!(chain, original);

        let mut chain = original.clone();
        assert!(chain.set_enabled(5, true).is_err());
        assert_eq!(chain, original);
    }

    #[test]
    fn move_uses_final_index_and_noops_succeed() {
        let mut chain = FilterChain::new(vec![
            entry("a", eq_filter(), true),
            entry("b", format_filter(), true),
            entry("c", hqdn3d_filter(), true),
        ])
        .unwrap();

        chain.move_entry(0, 2).unwrap();
        let labels: Vec<&str> = chain.entries().iter().map(|e| e.label()).collect();
        assert_eq!(labels, ["b", "c", "a"]);

        chain.move_entry(2, 0).unwrap();
        let labels: Vec<&str> = chain.entries().iter().map(|e| e.label()).collect();
        assert_eq!(labels, ["a", "b", "c"]);

        chain.move_entry(1, 1).unwrap();
        assert_eq!(chain.entries()[1].label(), "b");

        chain.rename(0, "a".to_string()).unwrap();
        chain.replace(0, entry("a", eq_filter(), false)).unwrap();
        assert!(!chain.entries()[0].enabled());
    }

    #[test]
    fn valid_edits_preserve_unaffected_payloads_flags_and_order() {
        let mut chain = FilterChain::new(vec![
            entry("a", eq_filter(), true),
            entry("b", format_filter(), false),
        ])
        .unwrap();
        chain.insert(2, entry("c", hqdn3d_filter(), false)).unwrap();
        chain.rename(1, "renamed".to_string()).unwrap();
        chain.set_filter(0, hqdn3d_filter()).unwrap();
        chain.set_enabled(2, true).unwrap();
        chain.move_entry(2, 0).unwrap();
        assert_eq!(
            chain.entries(),
            [
                entry("c", hqdn3d_filter(), true),
                entry("a", hqdn3d_filter(), true),
                entry("renamed", format_filter(), false),
            ]
        );
        assert_eq!(chain.remove(1).unwrap(), entry("a", hqdn3d_filter(), true));
        chain
            .replace(1, entry("replacement", eq_filter(), true))
            .unwrap();
        let expected = FilterChain::new(vec![
            entry("c", hqdn3d_filter(), true),
            entry("replacement", eq_filter(), true),
        ])
        .unwrap();
        assert_eq!(chain, expected);
        let json = serde_json::to_string(&chain).unwrap();
        assert_eq!(
            serde_json::from_str::<FilterChain>(&json).unwrap(),
            expected
        );
    }

    // --- serde roundtrip / strictness --------------------------------------

    #[test]
    fn chain_roundtrip_preserves_order_and_enabled() {
        let chain = FilterChain::new(vec![
            entry("g", format_filter(), true),
            entry("e", eq_filter(), false),
            entry("h", hqdn3d_filter(), true),
        ])
        .unwrap();
        let text = serde_json::to_string(&chain).unwrap();
        let back: FilterChain = serde_json::from_str(&text).unwrap();
        assert_eq!(back, chain);
        assert_eq!(back.entries()[1].label(), "e");
        assert!(!back.entries()[1].enabled());
    }

    #[test]
    fn serialized_filter_shape_is_kind_and_parameters() {
        let value = serde_json::to_value(eq_filter()).unwrap();
        assert_eq!(value["kind"], "eq");
        assert!(value["parameters"]["contrast"].is_number());
        assert!(value.get("raw").is_none());

        let value = serde_json::to_value(entry("L", eq_filter(), true)).unwrap();
        assert_eq!(value["label"], "L");
        assert_eq!(value["enabled"], true);
        assert_eq!(value["filter"]["kind"], "eq");

        let value = serde_json::to_value(FilterChain::default()).unwrap();
        assert!(value["entries"].is_array());
    }

    #[test]
    fn unknown_fields_and_kinds_rejected() {
        // unknown filter kind
        assert!(serde_json::from_str::<Filter>(r#"{"kind":"raw","parameters":{}}"#).is_err());
        // unknown outer field on filter envelope
        assert!(serde_json::from_str::<Filter>(
            r#"{"kind":"eq","parameters":{"contrast":1,"brightness":0,"saturation":1,"gamma":1,"gamma_r":1,"gamma_g":1,"gamma_b":1,"gamma_weight":1},"options":{}}"#
        )
        .is_err());
        // unknown nested parameter field
        assert!(serde_json::from_str::<Filter>(
            r#"{"kind":"eq","parameters":{"contrast":1,"brightness":0,"saturation":1,"gamma":1,"gamma_r":1,"gamma_g":1,"gamma_b":1,"gamma_weight":1,"graph":"x"}}"#
        )
        .is_err());
        // unknown entry field
        assert!(serde_json::from_str::<FilterEntry>(
            r#"{"label":"a","filter":{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}},"enabled":true,"path":"/x"}"#
        )
        .is_err());
        // unknown chain field
        assert!(serde_json::from_str::<FilterChain>(r#"{"entries":[],"command":"x"}"#).is_err());
        // positional sequence rejected
        assert!(serde_json::from_str::<FilterChain>("[]").is_err());
    }

    #[test]
    fn nested_duplicate_parameters_rejected_in_either_value_and_outer_key_order() {
        for contrasts in [
            r#""contrast":2000,"contrast":1"#,
            r#""contrast":1,"contrast":2000"#,
        ] {
            let parameters = format!(
                r#"{{{contrasts},"brightness":0,"saturation":1,"gamma":1,"gamma_r":1,"gamma_g":1,"gamma_b":1,"gamma_weight":1}}"#
            );
            assert!(serde_json::from_str::<EqParams>(&parameters).is_err());
            for filter in [
                format!(r#"{{"kind":"eq","parameters":{parameters}}}"#),
                format!(r#"{{"parameters":{parameters},"kind":"eq"}}"#),
            ] {
                assert!(serde_json::from_str::<Filter>(&filter).is_err());
                let entry = format!(r#"{{"label":"duplicate","enabled":false,"filter":{filter}}}"#);
                assert!(serde_json::from_str::<FilterEntry>(&entry).is_err());
                let chain = format!(r#"{{"entries":[{entry}]}}"#);
                assert!(serde_json::from_str::<FilterChain>(&chain).is_err());
            }
        }
    }

    #[test]
    fn valid_filter_parameters_may_precede_kind() {
        let parameters = serde_json::to_string(&EqParams::new(base_eq()).unwrap()).unwrap();
        let filter = format!(r#"{{"parameters":{parameters},"kind":"eq"}}"#);
        assert_eq!(
            serde_json::from_str::<Filter>(&filter).unwrap(),
            eq_filter()
        );
        let chain =
            format!(r#"{{"entries":[{{"label":"ordered","enabled":true,"filter":{filter}}}]}}"#);
        assert_eq!(
            serde_json::from_str::<FilterChain>(&chain).unwrap(),
            FilterChain::new(vec![entry("ordered", eq_filter(), true)]).unwrap()
        );
    }

    #[test]
    fn filter_and_parameters_require_strict_object_fields() {
        for filter in [
            r#"["format",{"matrix":"auto","levels":"auto","gamma":"auto"}]"#,
            r#"{"kind":"format","parameters":["auto","auto","auto"]}"#,
            r#"{"parameters":["auto","auto","auto"],"kind":"format"}"#,
            r#"{"kind":"format","kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}"#,
            r#"{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"},"parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}"#,
            r#"{"parameters":{"matrix":"auto","levels":"auto","gamma":"auto","extra":1},"kind":"format"}"#,
            r#"{"parameters":{"matrix":"auto","levels":"auto","gamma":"auto"},"kind":"format","extra":1}"#,
            r#"{"parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}"#,
            r#"{"kind":"format"}"#,
        ] {
            assert!(serde_json::from_str::<Filter>(filter).is_err());
        }
    }

    #[test]
    fn invalid_nested_params_rejected_even_when_disabled() {
        let json = r#"{"entries":[{"label":"a","enabled":false,"filter":{"kind":"eq","parameters":{"contrast":2000.0,"brightness":0,"saturation":1,"gamma":1,"gamma_r":1,"gamma_g":1,"gamma_b":1,"gamma_weight":1}}}]}"#;
        assert!(serde_json::from_str::<FilterChain>(json).is_err());
    }

    #[test]
    fn duplicate_json_labels_rejected_on_decode() {
        let json = r#"{"entries":[
            {"label":"x","enabled":true,"filter":{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}},
            {"label":"x","enabled":false,"filter":{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}}
        ]}"#;
        assert!(serde_json::from_str::<FilterChain>(json).is_err());
    }

    #[test]
    fn missing_enabled_field_rejected() {
        let json = r#"{"entries":[{"label":"a","filter":{"kind":"format","parameters":{"matrix":"auto","levels":"auto","gamma":"auto"}}}]}"#;
        assert!(serde_json::from_str::<FilterChain>(json).is_err());
    }

    #[test]
    fn unknown_enum_variant_rejected() {
        assert!(serde_json::from_str::<Filter>(
            r#"{"kind":"format","parameters":{"matrix":"bt2020","levels":"auto","gamma":"auto"}}"#
        )
        .is_err());
    }

    #[test]
    fn malicious_label_is_inert_and_roundtrips() {
        let label = "],lavfi=[movie=/path]\u{0}\"\\ λ";
        let chain = FilterChain::new(vec![entry(label, eq_filter(), true)]).unwrap();
        let text = serde_json::to_string(&chain).unwrap();
        let back: FilterChain = serde_json::from_str(&text).unwrap();
        assert_eq!(back.entries()[0].label(), label);
    }
}
