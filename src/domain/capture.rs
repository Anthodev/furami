//! Pure capture-domain model: USB identity, native video tuples and explicit
//! audio source identity, selection and playback gain.
//!
//! Allowed dependencies: `std`, `serde` (Serialize only), `thiserror`. No Qt, no
//! native pointers or Linux access. Ephemeral route authorizations carry physical
//! locators, never persisted identities. Fraction arithmetic is exact
//! (`u128` intermediates); no floats, no sampling, no cartesian products.

use std::num::{NonZeroU8, NonZeroU32, NonZeroU64};

use serde::Serialize;

use super::failure::ApplyFailure;

macro_rules! recovery_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
        pub struct $name(NonZeroU64);
        impl $name {
            pub fn new(value: u64) -> Option<Self> {
                NonZeroU64::new(value).map(Self)
            }
            pub fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}
recovery_id!(WatchId);
recovery_id!(ObservationEpoch);
recovery_id!(CandidateId);
recovery_id!(AudioEpoch);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct WatchStamp {
    pub watch: WatchId,
    pub epoch: ObservationEpoch,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RecoveryWatchTarget {
    pub watch: WatchId,
    pub video: ModeRequest,
    pub audio: AudioSelection,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SelectionToken {
    pub stamp: WatchStamp,
    pub candidate: CandidateId,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RecoveryCandidate {
    pub token: SelectionToken,
    pub identity: DeviceIdentity,
    /// Display only; never an identity match key.
    pub description: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum VideoPresence {
    Present,
    Absent,
    Ambiguous(Vec<RecoveryCandidate>),
    Unknown(ApplyFailure),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum SourcePresence {
    Disabled,
    Present,
    Absent(AudioError),
    Unknown(AudioError),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RecoveryObservation {
    pub stamp: WatchStamp,
    pub video: VideoPresence,
    pub audio: SourcePresence,
    /// Monotonic removal high-water mark, retained across coalesced replug scans.
    pub last_video_removal: Option<ObservationEpoch>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum LossEvidence {
    StreamEnded { reason: i32, error: i32 },
    Removed { stamp: WatchStamp },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum AudioSilence {
    WaitingForSource(AudioError),
    PendingRoute,
    Paused,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AudioRouteReceipt {
    pub epoch: AudioEpoch,
    pub stamp: WatchStamp,
    pub source_index: u32,
    pub source_output_index: u32,
    pub client_index: u32,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum AudioAvailability {
    Disabled,
    Silent {
        reason: AudioSilence,
    },
    Opening {
        epoch: AudioEpoch,
    },
    Active {
        source: AudioSourceIdentity,
        route: AudioRouteReceipt,
    },
    Detaching {
        epoch: AudioEpoch,
    },
    Blocked {
        epoch: AudioEpoch,
        error: AudioError,
    },
}

/// One-open authorization produced only by successful fresh capture validation.
/// Physical syspaths are locators, never persisted device identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedRouteAuthorization {
    pub token: SelectionToken,
    pub identity: DeviceIdentity,
    pub usb_syspath: std::path::PathBuf,
    pub node_syspath: std::path::PathBuf,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Invalid capture data. Each variant distinguishes one kind of invalid datum,
/// never an absence of support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
pub enum CaptureDataError {
    #[error("invalid capture identity field `{field}`")]
    InvalidIdentity { field: &'static str },
    #[error("zero value in rational field `{field}`")]
    ZeroRational { field: &'static str },
    #[error("invalid frame rate value `{value}`")]
    InvalidRate { value: String },
    #[error("invalid capture descriptor: {reason}")]
    InvalidDescriptor { reason: &'static str },
}

/// Identity-resolution failure against an observed snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
pub enum IdentityError {
    #[error("no observed capture device matches the requested identity")]
    NotFound,
    #[error("{candidates} serial-less devices share the requested VID/PID")]
    SeriallessDuplicates { candidates: usize },
    #[error("{candidates} devices report the requested serial")]
    DuplicateSerial { candidates: usize },
}

/// Owned Pulse failures. Silence and a suspended source are not failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
pub enum AudioError {
    #[error("audio service unavailable: {0}")]
    Unavailable(String),
    #[error("invalid audio selection: {0}")]
    InvalidSelection(String),
    #[error("selected audio source `{name}` is unavailable")]
    SourceMissing { name: String },
    #[error("selected audio source `{name}` changed identity")]
    SourceChanged { name: String },
    #[error("owned audio recording transport lost: {detail}")]
    RecordingLost { detail: String },
    #[error("ambiguous audio ownership: {0}")]
    Ambiguous(String),
    #[error("audio opening cancelled")]
    Cancelled,
    #[error("audio cancellation failed: {0}")]
    Control(String),
    #[error("audio backend {operation} failed (code {code:?}): {detail}")]
    Backend {
        operation: String,
        code: Option<i32>,
        detail: String,
    },
}

/// Exact source name plus stable properties observed when selected. Server
/// indices, ALSA card numbers, running/suspended state and mixer values are not
/// identity. Missing previously observed properties fail revalidation closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioSourceIdentity {
    name: String,
    stable_properties: Vec<(String, String)>,
}

impl AudioSourceIdentity {
    pub fn new(
        name: String,
        mut stable_properties: Vec<(String, String)>,
    ) -> Result<Self, AudioError> {
        if name.trim().is_empty() || name.contains('\0') || name.starts_with('@') {
            return Err(AudioError::InvalidSelection(
                "explicit source name required".into(),
            ));
        }
        stable_properties.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if stable_properties.iter().any(|(key, value)| {
            key.is_empty() || value.is_empty() || key.contains('\0') || value.contains('\0')
        }) || stable_properties
            .windows(2)
            .any(|pair| pair[0].0 == pair[1].0)
        {
            return Err(AudioError::InvalidSelection(
                "invalid stable source properties".into(),
            ));
        }
        Ok(Self {
            name,
            stable_properties,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn compatible_with(&self, observed: &Self) -> bool {
        self.name == observed.name
            && self.stable_properties.iter().all(|property| {
                observed
                    .stable_properties
                    .binary_search_by(|other| other.0.cmp(&property.0))
                    .is_ok_and(|index| observed.stable_properties[index].1 == property.1)
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum AudioSelection {
    Disabled {
        retained: Option<AudioSourceIdentity>,
    },
    Enabled {
        source: AudioSourceIdentity,
    },
}

impl Default for AudioSelection {
    fn default() -> Self {
        Self::Disabled { retained: None }
    }
}

impl AudioSelection {
    /// Includes the retained disabled selection, never an implicit default.
    pub fn source(&self) -> Option<&AudioSourceIdentity> {
        match self {
            Self::Disabled { retained } => retained.as_ref(),
            Self::Enabled { source } => Some(source),
        }
    }

    pub fn enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }
}

/// Playback-only gain; never applied to the capture-source mixer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlaybackGain {
    pub volume_percent: u8,
    pub muted: bool,
}

impl Default for PlaybackGain {
    fn default() -> Self {
        Self {
            volume_percent: 100,
            muted: false,
        }
    }
}

impl PlaybackGain {
    pub fn new(volume_percent: u8, muted: bool) -> Result<Self, AudioError> {
        if volume_percent > 100 {
            return Err(AudioError::InvalidSelection(
                "playback volume must be 0..=100".into(),
            ));
        }
        Ok(Self {
            volume_percent,
            muted,
        })
    }
}

// ---------------------------------------------------------------------------
// USB identity
// ---------------------------------------------------------------------------

/// Physical USB attachment: host controller plus the port chain below it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsbTopology {
    controller: String,
    ports: Vec<NonZeroU8>,
}

impl UsbTopology {
    pub fn new(controller: String, ports: Vec<NonZeroU8>) -> Result<Self, CaptureDataError> {
        if controller.is_empty() || controller.trim().is_empty() {
            return Err(CaptureDataError::InvalidIdentity {
                field: "controller",
            });
        }
        if controller.contains('/') {
            return Err(CaptureDataError::InvalidIdentity {
                field: "controller",
            });
        }
        if ports.is_empty() {
            return Err(CaptureDataError::InvalidIdentity { field: "ports" });
        }
        Ok(Self { controller, ports })
    }

    pub fn controller(&self) -> &str {
        &self.controller
    }

    pub fn ports(&self) -> &[NonZeroU8] {
        &self.ports
    }
}

/// Technical identity of one physical capture adapter. Never carries a human
/// name, a `/dev/videoN` node number or a session path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceIdentity {
    vendor_id: u16,
    product_id: u16,
    topology: UsbTopology,
    serial: Option<String>,
}

impl DeviceIdentity {
    pub fn new(
        vendor_id: u16,
        product_id: u16,
        topology: UsbTopology,
        serial: Option<String>,
    ) -> Result<Self, CaptureDataError> {
        if serial.as_deref().is_some_and(str::is_empty) {
            return Err(CaptureDataError::InvalidIdentity { field: "serial" });
        }
        Ok(Self {
            vendor_id,
            product_id,
            topology,
            serial,
        })
    }

    pub fn vendor_id(&self) -> u16 {
        self.vendor_id
    }

    pub fn product_id(&self) -> u16 {
        self.product_id
    }

    pub fn topology(&self) -> &UsbTopology {
        &self.topology
    }

    pub fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Rationals and FourCC
// ---------------------------------------------------------------------------

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Free-form kernel FourCC: the 32 bits announced by V4L2, kept verbatim with
/// little-endian conversion. Never mapped onto a decoded-format qualification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct CapturedFourCc(u32);

impl CapturedFourCc {
    pub const fn from_kernel(value: u32) -> Self {
        Self(value)
    }

    pub const fn from_bytes(value: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(value))
    }

    pub const fn kernel_value(self) -> u32 {
        self.0
    }

    pub const fn bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }
}

/// A width/height pair, both strictly positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FrameSize {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl FrameSize {
    pub fn new(width: u32, height: u32) -> Result<Self, CaptureDataError> {
        Ok(Self {
            width: NonZeroU32::new(width)
                .ok_or(CaptureDataError::ZeroRational { field: "width" })?,
            height: NonZeroU32::new(height)
                .ok_or(CaptureDataError::ZeroRational { field: "height" })?,
        })
    }

    pub fn width(&self) -> u32 {
        self.width.get()
    }

    pub fn height(&self) -> u32 {
        self.height.get()
    }
}

/// A frame period in seconds, normalized by GCD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FrameInterval {
    numerator: NonZeroU32,
    denominator: NonZeroU32,
}

impl FrameInterval {
    pub fn new(numerator: u32, denominator: u32) -> Result<Self, CaptureDataError> {
        let numerator = NonZeroU32::new(numerator)
            .ok_or(CaptureDataError::ZeroRational { field: "numerator" })?;
        let denominator = NonZeroU32::new(denominator).ok_or(CaptureDataError::ZeroRational {
            field: "denominator",
        })?;
        let g = gcd(numerator.get(), denominator.get());
        Ok(Self {
            numerator: NonZeroU32::new(numerator.get() / g)
                .ok_or(CaptureDataError::ZeroRational { field: "numerator" })?,
            denominator: NonZeroU32::new(denominator.get() / g).ok_or(
                CaptureDataError::ZeroRational {
                    field: "denominator",
                },
            )?,
        })
    }

    pub fn numerator(&self) -> u32 {
        self.numerator.get()
    }

    pub fn denominator(&self) -> u32 {
        self.denominator.get()
    }

    /// Exact frame rate: the reciprocal of the interval in seconds.
    pub fn rate(self) -> FrameRate {
        FrameRate {
            numerator: self.denominator,
            denominator: self.numerator,
        }
    }
}

/// A frame rate in frames per second, normalized by GCD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FrameRate {
    numerator: NonZeroU32,
    denominator: NonZeroU32,
}

impl FrameRate {
    pub fn new(numerator: u32, denominator: u32) -> Result<Self, CaptureDataError> {
        let numerator = NonZeroU32::new(numerator)
            .ok_or(CaptureDataError::ZeroRational { field: "numerator" })?;
        let denominator = NonZeroU32::new(denominator).ok_or(CaptureDataError::ZeroRational {
            field: "denominator",
        })?;
        let g = gcd(numerator.get(), denominator.get());
        Ok(Self {
            numerator: NonZeroU32::new(numerator.get() / g)
                .ok_or(CaptureDataError::ZeroRational { field: "numerator" })?,
            denominator: NonZeroU32::new(denominator.get() / g).ok_or(
                CaptureDataError::ZeroRational {
                    field: "denominator",
                },
            )?,
        })
    }

    /// Parses exactly `N` or `N/D` as unsigned decimal. Rejects floats, signs,
    /// whitespace, junk and u32 overflow. The fraction is normalized by GCD.
    pub fn parse_exact(value: &str) -> Result<Self, CaptureDataError> {
        let invalid = || CaptureDataError::InvalidRate {
            value: value.to_string(),
        };
        let (num_raw, den_raw) = match value.split_once('/') {
            Some((n, d)) => (n, d),
            None => (value, "1"),
        };
        if num_raw.is_empty()
            || den_raw.is_empty()
            || !num_raw
                .bytes()
                .chain(den_raw.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return Err(invalid());
        }
        let numerator: u32 = num_raw.parse().map_err(|_| invalid())?;
        let denominator: u32 = den_raw.parse().map_err(|_| invalid())?;
        FrameRate::new(numerator, denominator).map_err(|_| invalid())
    }

    pub fn numerator(&self) -> u32 {
        self.numerator.get()
    }

    pub fn denominator(&self) -> u32 {
        self.denominator.get()
    }

    /// Exact frame period: the reciprocal of the rate.
    pub fn interval(self) -> FrameInterval {
        FrameInterval {
            numerator: self.denominator,
            denominator: self.numerator,
        }
    }
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

/// V4L2 buffer queue layout of a capture node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CaptureBufferType {
    SinglePlanar,
    MultiPlanar,
}

/// One requested capture tuple: FourCC, size and exact rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CaptureMode {
    pub captured_fourcc: CapturedFourCc,
    pub size: FrameSize,
    pub rate: FrameRate,
}

/// A full request: which physical device, which mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModeRequest {
    pub identity: DeviceIdentity,
    pub mode: CaptureMode,
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

/// A descriptor the kernel either reports or does not report at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Descriptor<T> {
    Available(T),
    NotReported,
}

impl<T> Descriptor<T> {
    pub fn as_option(&self) -> Option<&T> {
        match self {
            Descriptor::Available(value) => Some(value),
            Descriptor::NotReported => None,
        }
    }
}

/// Native frame intervals for one FourCC, in the kernel's own terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameIntervals(FrameIntervalKind);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FrameIntervalKind {
    Discrete(Vec<FrameInterval>),
    Stepwise {
        min: FrameInterval,
        max: FrameInterval,
        step: FrameInterval,
    },
    Continuous {
        min: FrameInterval,
        max: FrameInterval,
    },
}

impl FrameIntervals {
    pub fn new(kind: FrameIntervalKind) -> Result<Self, CaptureDataError> {
        match &kind {
            FrameIntervalKind::Discrete(intervals) => {
                if intervals.is_empty() {
                    return Err(CaptureDataError::InvalidDescriptor {
                        reason: "empty discrete interval list",
                    });
                }
                for (i, a) in intervals.iter().enumerate() {
                    for b in &intervals[i + 1..] {
                        if a == b {
                            return Err(CaptureDataError::InvalidDescriptor {
                                reason: "duplicate discrete interval",
                            });
                        }
                    }
                }
            }
            FrameIntervalKind::Stepwise { min, max, .. }
            | FrameIntervalKind::Continuous { min, max } => {
                // Inclusive bounds compared exactly in u128: min must not exceed max.
                let (a, b, c, d) = (
                    u128::from(min.numerator()),
                    u128::from(min.denominator()),
                    u128::from(max.numerator()),
                    u128::from(max.denominator()),
                );
                if a * d > c * b {
                    return Err(CaptureDataError::InvalidDescriptor {
                        reason: "inverted interval bounds",
                    });
                }
            }
        }
        Ok(Self(kind))
    }

    pub fn kind(&self) -> &FrameIntervalKind {
        &self.0
    }

    /// Exact membership test. Discrete: the rate must equal one announced
    /// interval's reciprocal. Stepwise: inclusive bounds, then the requested
    /// period must sit on the step lattice measured in seconds. Continuous:
    /// inclusive bounds only. All intermediates are u128.
    pub fn supports(&self, rate: FrameRate) -> bool {
        let interval = rate.interval();
        let (a, b) = (interval.numerator(), interval.denominator());
        match &self.0 {
            FrameIntervalKind::Discrete(intervals) => intervals.iter().any(|interval| {
                u128::from(interval.numerator()) * u128::from(b)
                    == u128::from(a) * u128::from(interval.denominator())
            }),
            FrameIntervalKind::Stepwise { min, max, step } => {
                let (c, d) = (min.numerator(), min.denominator());
                let (e, f) = (max.numerator(), max.denominator());
                let (g, h) = (step.numerator(), step.denominator());
                // Inclusive bounds: a*d >= c*b and a*f <= e*b, then the offset
                // from min must be a whole multiple of the step, in seconds.
                let bounds_ok = u128::from(a) * u128::from(d) >= u128::from(c) * u128::from(b)
                    && u128::from(a) * u128::from(f) <= u128::from(e) * u128::from(b);
                bounds_ok
                    && ((u128::from(a) * u128::from(d) - u128::from(c) * u128::from(b))
                        * u128::from(h))
                        % (u128::from(b) * u128::from(d) * u128::from(g))
                        == 0
            }
            FrameIntervalKind::Continuous { min, max } => {
                let (c, d) = (min.numerator(), min.denominator());
                let (e, f) = (max.numerator(), max.denominator());
                u128::from(a) * u128::from(d) >= u128::from(c) * u128::from(b)
                    && u128::from(a) * u128::from(f) <= u128::from(e) * u128::from(b)
            }
        }
    }
}

/// One discrete size and its interval descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscreteSize {
    pub size: FrameSize,
    pub intervals: Descriptor<FrameIntervals>,
}

/// Native frame sizes for one FourCC, in the kernel's own terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrameSizes(FrameSizeKind);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FrameSizeKind {
    Discrete(Vec<DiscreteSize>),
    Stepwise {
        min: FrameSize,
        max: FrameSize,
        step_width: NonZeroU32,
        step_height: NonZeroU32,
    },
    Continuous {
        min: FrameSize,
        max: FrameSize,
    },
}

impl FrameSizes {
    pub fn new(kind: FrameSizeKind) -> Result<Self, CaptureDataError> {
        match &kind {
            FrameSizeKind::Discrete(sizes) => {
                if sizes.is_empty() {
                    return Err(CaptureDataError::InvalidDescriptor {
                        reason: "empty discrete size list",
                    });
                }
                for (i, a) in sizes.iter().enumerate() {
                    for b in &sizes[i + 1..] {
                        if a.size == b.size {
                            return Err(CaptureDataError::InvalidDescriptor {
                                reason: "duplicate discrete size",
                            });
                        }
                    }
                }
            }
            FrameSizeKind::Stepwise {
                min,
                max,
                step_width,
                step_height,
            } => {
                if min.width() > max.width()
                    || min.height() > max.height()
                    || step_width.get() == 0
                    || step_height.get() == 0
                {
                    return Err(CaptureDataError::InvalidDescriptor {
                        reason: "invalid stepwise size range",
                    });
                }
            }
            FrameSizeKind::Continuous { min, max } => {
                if min.width() > max.width() || min.height() > max.height() {
                    return Err(CaptureDataError::InvalidDescriptor {
                        reason: "inverted continuous size bounds",
                    });
                }
            }
        }
        Ok(Self(kind))
    }

    pub fn kind(&self) -> &FrameSizeKind {
        &self.0
    }

    /// Exact membership. Discrete: one announced size matches. Stepwise:
    /// inclusive bounds, then independent width/height modulo after a safe
    /// subtraction. Continuous: inclusive bounds only. No sampling.
    pub fn contains(&self, size: FrameSize) -> bool {
        match &self.0 {
            FrameSizeKind::Discrete(sizes) => sizes.iter().any(|entry| entry.size == size),
            FrameSizeKind::Stepwise {
                min,
                max,
                step_width,
                step_height,
            } => {
                (min.width()..=max.width()).contains(&size.width())
                    && (min.height()..=max.height()).contains(&size.height())
                    && (size.width() - min.width()).is_multiple_of(step_width.get())
                    && (size.height() - min.height()).is_multiple_of(step_height.get())
            }
            FrameSizeKind::Continuous { min, max } => {
                (min.width()..=max.width()).contains(&size.width())
                    && (min.height()..=max.height()).contains(&size.height())
            }
        }
    }
}

/// One format announced by one buffer queue of a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormatDescriptor {
    pub buffer_type: CaptureBufferType,
    pub captured_fourcc: CapturedFourCc,
    pub description: String,
    pub flags: u32,
}

/// All native size/interval descriptors for one distinct FourCC of a node.
/// The kernel does not key frame sizes or intervals by buffer type, so these
/// are shared by FourCC within a node — never between nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FourCcCapabilities {
    pub captured_fourcc: CapturedFourCc,
    pub sizes: Descriptor<FrameSizes>,
}

/// The validated capability set of one capture node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeCapabilities {
    formats: Vec<FormatDescriptor>,
    fourcc_capabilities: Vec<FourCcCapabilities>,
}

/// Why a mode is not supported, at the most precise level reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum UnsupportedReason {
    FourCc,
    FrameSize,
    FrameRate,
}

/// Why a verdict cannot be produced from the snapshot alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum UnknownReason {
    FrameSizesNotReported,
    FrameIntervalsNotReported,
}

/// Verdict for one (buffer type, mode) against one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SupportVerdict {
    Supported,
    Unsupported(UnsupportedReason),
    /// The size lies inside a native range: exact intervals for that size must
    /// be queried before a verdict.
    NeedsExactIntervals,
    Unknown(UnknownReason),
}

impl NodeCapabilities {
    /// `formats` holds every format actually announced per buffer queue;
    /// `caps` must hold exactly one entry per distinct format FourCC — no
    /// orphan, missing or duplicated entry.
    pub fn new(
        formats: Vec<FormatDescriptor>,
        caps: Vec<FourCcCapabilities>,
    ) -> Result<Self, CaptureDataError> {
        if formats.is_empty() {
            return Err(CaptureDataError::InvalidDescriptor {
                reason: "empty format list",
            });
        }
        for (i, a) in formats.iter().enumerate() {
            for b in &formats[i + 1..] {
                if a.buffer_type == b.buffer_type && a.captured_fourcc == b.captured_fourcc {
                    return Err(CaptureDataError::InvalidDescriptor {
                        reason: "duplicate format entry",
                    });
                }
            }
        }
        // No temporary distinct-FourCC list: reject duplicated capability
        // entries, then require each format to have its entry and each entry
        // to be announced by some format.
        for (i, a) in caps.iter().enumerate() {
            for b in &caps[i + 1..] {
                if a.captured_fourcc == b.captured_fourcc {
                    return Err(CaptureDataError::InvalidDescriptor {
                        reason: "duplicated capability entry",
                    });
                }
            }
        }
        for format in &formats {
            if !caps
                .iter()
                .any(|entry| entry.captured_fourcc == format.captured_fourcc)
            {
                return Err(CaptureDataError::InvalidDescriptor {
                    reason: "capability entry missing for a format",
                });
            }
        }
        for entry in &caps {
            if !formats
                .iter()
                .any(|format| format.captured_fourcc == entry.captured_fourcc)
            {
                return Err(CaptureDataError::InvalidDescriptor {
                    reason: "orphan capability entry",
                });
            }
        }
        Ok(Self {
            formats,
            fourcc_capabilities: caps,
        })
    }

    pub fn formats(&self) -> &[FormatDescriptor] {
        &self.formats
    }

    pub fn fourcc_capabilities(&self) -> &[FourCcCapabilities] {
        &self.fourcc_capabilities
    }

    /// Validates the FourCC → size → interval relations for one mode on one
    /// buffer queue. Produces a verdict only; never a tuple list or a product.
    pub fn assess(&self, buffer_type: CaptureBufferType, mode: CaptureMode) -> SupportVerdict {
        let announced = self.formats.iter().any(|format| {
            format.buffer_type == buffer_type && format.captured_fourcc == mode.captured_fourcc
        });
        if !announced {
            return SupportVerdict::Unsupported(UnsupportedReason::FourCc);
        }
        let entry = self
            .fourcc_capabilities
            .iter()
            .find(|entry| entry.captured_fourcc == mode.captured_fourcc);
        let Some(entry) = entry else {
            // Constructor invariant forbids this; verdict stays total regardless.
            return SupportVerdict::Unsupported(UnsupportedReason::FourCc);
        };
        let Some(sizes) = entry.sizes.as_option() else {
            return SupportVerdict::Unknown(UnknownReason::FrameSizesNotReported);
        };
        match sizes.kind() {
            FrameSizeKind::Discrete(discrete) => {
                let Some(found) = discrete.iter().find(|entry| entry.size == mode.size) else {
                    return SupportVerdict::Unsupported(UnsupportedReason::FrameSize);
                };
                match &found.intervals {
                    Descriptor::NotReported => {
                        SupportVerdict::Unknown(UnknownReason::FrameIntervalsNotReported)
                    }
                    Descriptor::Available(intervals) => {
                        if intervals.supports(mode.rate) {
                            SupportVerdict::Supported
                        } else {
                            SupportVerdict::Unsupported(UnsupportedReason::FrameRate)
                        }
                    }
                }
            }
            // Sizes inside a native range need a per-size exact query; the
            // range is never treated as a full enumeration.
            FrameSizeKind::Stepwise { .. } | FrameSizeKind::Continuous { .. } => {
                if sizes.contains(mode.size) {
                    SupportVerdict::NeedsExactIntervals
                } else {
                    SupportVerdict::Unsupported(UnsupportedReason::FrameSize)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Identity resolution
// ---------------------------------------------------------------------------

/// Resolves a requested identity against observed device identities and
/// returns the index of the match **in the original observation order**.
///
/// - With a serial: exact serial match **within the same VID/PID**; zero
///   matches is [`IdentityError::NotFound`] (no topology fallback), more than
///   one is [`IdentityError::DuplicateSerial`]. A unique serial match ignores
///   topology changes (the device may have moved).
/// - Without a serial: count serial-less candidates with the same VID/PID.
///   More than one is [`IdentityError::SeriallessDuplicates`] — ambiguity is
///   reported before any port filtering, even on distinct ports. A single
///   candidate must keep the same topology, else [`IdentityError::NotFound`].
///
/// The iterator is consumed lazily; no identity list is cloned or allocated.
pub fn resolve_identity<'a>(
    requested: &DeviceIdentity,
    observed: impl IntoIterator<Item = &'a DeviceIdentity>,
) -> Result<usize, IdentityError> {
    match requested.serial.as_deref() {
        Some(serial) => {
            let mut found: Option<(usize, &'a DeviceIdentity)> = None;
            let mut count = 0usize;
            for (index, device) in observed.into_iter().enumerate() {
                if device.serial.as_deref() == Some(serial)
                    && device.vendor_id == requested.vendor_id
                    && device.product_id == requested.product_id
                {
                    count += 1;
                    found = Some((index, device));
                }
            }
            match count {
                0 => Err(IdentityError::NotFound),
                1 => Ok(found.map_or(usize::MAX, |(index, _)| index)),
                _ => Err(IdentityError::DuplicateSerial { candidates: count }),
            }
        }
        None => {
            let mut found: Option<(usize, &'a DeviceIdentity)> = None;
            let mut count = 0usize;
            for (index, device) in observed.into_iter().enumerate() {
                if device.serial.is_none()
                    && device.vendor_id == requested.vendor_id
                    && device.product_id == requested.product_id
                {
                    count += 1;
                    found = Some((index, device));
                }
            }
            if count > 1 {
                return Err(IdentityError::SeriallessDuplicates { candidates: count });
            }
            match found {
                None => Err(IdentityError::NotFound),
                Some((index, device)) => {
                    if device.topology == requested.topology {
                        Ok(index)
                    } else {
                        Err(IdentityError::NotFound)
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_identity_ignores_new_properties_but_never_drops_known_identity() {
        let selected = AudioSourceIdentity::new(
            "capture".into(),
            vec![("device.serial".into(), "serial".into())],
        )
        .unwrap();
        let enriched = AudioSourceIdentity::new(
            "capture".into(),
            vec![
                ("device.serial".into(), "serial".into()),
                ("device.bus".into(), "usb".into()),
            ],
        )
        .unwrap();
        assert!(selected.compatible_with(&enriched));
        assert!(
            !selected.compatible_with(&AudioSourceIdentity::new("capture".into(), vec![]).unwrap())
        );
        assert!(
            !selected.compatible_with(
                &AudioSourceIdentity::new(
                    "capture".into(),
                    vec![("device.serial".into(), "other".into())],
                )
                .unwrap()
            )
        );
    }

    #[test]
    fn audio_identity_rejects_default_aliases_nul_and_duplicate_keys() {
        for name in ["", "@DEFAULT_SOURCE@", "@DEFAULT_MONITOR@", "bad\0name"] {
            assert!(AudioSourceIdentity::new(name.into(), vec![]).is_err());
        }
        assert!(
            AudioSourceIdentity::new(
                "capture".into(),
                vec![
                    ("device.serial".into(), "a".into()),
                    ("device.serial".into(), "b".into())
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn disabled_audio_retains_selection_without_enabling_capture() {
        let source = AudioSourceIdentity::new("capture".into(), vec![]).unwrap();
        let selection = AudioSelection::Disabled {
            retained: Some(source.clone()),
        };
        assert!(!selection.enabled());
        assert_eq!(selection.source(), Some(&source));
        assert_eq!(AudioSelection::default().source(), None);
    }

    #[test]
    fn playback_gain_rejects_amplification_and_preserves_muting() {
        assert!(PlaybackGain::new(101, false).is_err());
        assert_eq!(PlaybackGain::new(0, true).unwrap().volume_percent, 0);
        assert!(PlaybackGain::new(75, true).unwrap().muted);
        assert_eq!(PlaybackGain::default().volume_percent, 100);
    }

    const NV12: CapturedFourCc = CapturedFourCc::from_bytes(*b"NV12");
    const YUYV: CapturedFourCc = CapturedFourCc::from_bytes(*b"YUYV");

    fn size(w: u32, h: u32) -> FrameSize {
        FrameSize::new(w, h).unwrap()
    }

    fn rate(n: u32, d: u32) -> FrameRate {
        FrameRate::new(n, d).unwrap()
    }

    fn interval(n: u32, d: u32) -> FrameInterval {
        FrameInterval::new(n, d).unwrap()
    }

    fn discrete_intervals(list: &[FrameInterval]) -> FrameIntervals {
        FrameIntervals::new(FrameIntervalKind::Discrete(list.to_vec())).unwrap()
    }

    fn discrete_sizes(list: &[(u32, u32, Descriptor<FrameIntervals>)]) -> FrameSizes {
        FrameSizes::new(FrameSizeKind::Discrete(
            list.iter()
                .map(|(w, h, intervals)| DiscreteSize {
                    size: size(*w, *h),
                    intervals: intervals.clone(),
                })
                .collect(),
        ))
        .unwrap()
    }

    fn format(buffer_type: CaptureBufferType, fourcc: CapturedFourCc) -> FormatDescriptor {
        FormatDescriptor {
            buffer_type,
            captured_fourcc: fourcc,
            description: "desc".to_string(),
            flags: 0,
        }
    }

    fn caps_entry(fourcc: CapturedFourCc, sizes: Descriptor<FrameSizes>) -> FourCcCapabilities {
        FourCcCapabilities {
            captured_fourcc: fourcc,
            sizes,
        }
    }

    fn identity(serial: Option<&str>, controller: &str, ports: &[u8]) -> DeviceIdentity {
        DeviceIdentity::new(
            0x32ed,
            0x3701,
            UsbTopology::new(
                controller.to_string(),
                ports.iter().map(|&p| NonZeroU8::new(p).unwrap()).collect(),
            )
            .unwrap(),
            serial.map(str::to_string),
        )
        .unwrap()
    }

    // -- FourCC -------------------------------------------------------------

    #[test]
    fn fourcc_roundtrips_through_kernel_value_and_bytes() {
        let fourcc = CapturedFourCc::from_bytes(*b"NV12");
        assert_eq!(fourcc.kernel_value(), 0x3231_564E);
        assert_eq!(fourcc.bytes(), *b"NV12");
        assert_eq!(CapturedFourCc::from_kernel(0x3231_564E).bytes(), *b"NV12");
    }

    // -- Rationals ----------------------------------------------------------

    #[test]
    fn fractions_are_gcd_normalized() {
        assert_eq!(interval(1920, 1080), interval(16, 9));
        assert_eq!(rate(1920, 1080), rate(16, 9));
        assert_ne!(rate(60, 1), rate(60000, 1001));
    }

    #[test]
    fn zero_rationals_are_rejected() {
        assert_eq!(
            FrameRate::new(0, 1),
            Err(CaptureDataError::ZeroRational { field: "numerator" })
        );
        assert_eq!(
            FrameRate::new(60, 0),
            Err(CaptureDataError::ZeroRational {
                field: "denominator"
            })
        );
        assert_eq!(
            FrameInterval::new(1, 0),
            Err(CaptureDataError::ZeroRational {
                field: "denominator"
            })
        );
        assert_eq!(
            FrameSize::new(0, 100),
            Err(CaptureDataError::ZeroRational { field: "width" })
        );
        assert_eq!(
            FrameSize::new(100, 0),
            Err(CaptureDataError::ZeroRational { field: "height" })
        );
    }

    #[test]
    fn interval_rate_reciprocals_are_exact() {
        assert_eq!(interval(1001, 60000).rate(), rate(60000, 1001));
        assert_eq!(rate(60000, 1001).interval(), interval(1001, 60000));
        assert_eq!(rate(60, 1).interval(), interval(1, 60));
        assert_eq!(interval(1, 60).rate(), rate(60, 1));
    }

    #[test]
    fn parse_exact_accepts_only_unsigned_decimal_n_or_n_over_d() {
        assert_eq!(FrameRate::parse_exact("60"), Ok(rate(60, 1)));
        assert_eq!(FrameRate::parse_exact("60/1"), Ok(rate(60, 1)));
        assert_eq!(FrameRate::parse_exact("60000/1001"), Ok(rate(60000, 1001)));
        assert_eq!(FrameRate::parse_exact("001/2"), Ok(rate(1, 2)));
        for bad in [
            "60.0",
            "-1",
            "+1",
            " 60",
            "60 ",
            "1 /2",
            "abc",
            "",
            "60/",
            "/60",
            "60/1/2",
            "1/0",
            "0/1",
            "4294967296",
            "4294967295/4294967296",
        ] {
            assert_eq!(
                FrameRate::parse_exact(bad),
                Err(CaptureDataError::InvalidRate {
                    value: bad.to_string()
                }),
                "expected rejection of `{bad}`"
            );
        }
        assert_eq!(
            FrameRate::parse_exact("4294967295"),
            Ok(rate(4294967295, 1))
        );
    }

    // -- Interval support ---------------------------------------------------

    #[test]
    fn discrete_support_is_exact_membership() {
        let intervals =
            discrete_intervals(&[interval(1, 60), interval(1, 50), interval(1001, 60000)]);
        assert!(intervals.supports(rate(60, 1)));
        assert!(intervals.supports(rate(60000, 1001)));
        assert!(intervals.supports(rate(120, 2))); // equal to 60/1 after GCD
        assert!(!intervals.supports(rate(30, 1)));
        assert!(!intervals.supports(rate(59, 1)));
    }

    #[test]
    fn rate_new_preserves_numerator_and_denominator() {
        let sixty = rate(60, 1);
        assert_eq!(sixty.numerator(), 60);
        assert_eq!(sixty.denominator(), 1);
        let parsed = FrameRate::parse_exact("60000/1001").unwrap();
        assert_eq!(parsed.numerator(), 60000);
        assert_eq!(parsed.denominator(), 1001);
        let gcd_case = rate(1920, 1080);
        assert_eq!((gcd_case.numerator(), gcd_case.denominator()), (16, 9));
        // Reciprocals stay exact in both directions.
        let interval = interval(1001, 60000);
        assert_eq!(
            (interval.numerator(), interval.denominator()),
            (1001, 60000)
        );
        let up = interval.rate();
        assert_eq!((up.numerator(), up.denominator()), (60000, 1001));
        let down = up.interval();
        assert_eq!((down.numerator(), down.denominator()), (1001, 60000));
    }

    #[test]
    fn stepwise_support_checks_bounds_then_step_in_seconds() {
        // min period 10/300 s (=1/30), max 60/300 s (=1/5), step 1/300 s.
        let intervals = FrameIntervals::new(FrameIntervalKind::Stepwise {
            min: interval(10, 300),
            max: interval(60, 300),
            step: interval(1, 300),
        })
        .unwrap();
        assert!(intervals.supports(rate(30, 1))); // min edge
        assert!(intervals.supports(rate(5, 1))); // max edge
        assert!(intervals.supports(rate(25, 1))); // period 12/300 s: on lattice
        assert!(!intervals.supports(rate(31, 1))); // above max rate
        assert!(!intervals.supports(rate(4, 1))); // below min rate
        // Period 7/200 s sits strictly between two 1/300 s lattice points.
        assert!(!intervals.supports(rate(200, 7))); // off lattice
    }

    #[test]
    fn continuous_support_is_bounds_only() {
        let intervals = FrameIntervals::new(FrameIntervalKind::Continuous {
            min: interval(1, 60),
            max: interval(1, 25),
        })
        .unwrap();
        assert!(intervals.supports(rate(60, 1)));
        assert!(intervals.supports(rate(25, 1)));
        assert!(intervals.supports(rate(31, 1))); // any value inside
        assert!(!intervals.supports(rate(61, 1)));
        assert!(!intervals.supports(rate(24, 1)));
    }

    #[test]
    fn range_arithmetic_survives_components_near_u32_max() {
        const N: u32 = u32::MAX;
        // Fixtures: min interval 1/N, max 1/1, step (N-2)/(N-1). All normalized.
        let intervals = FrameIntervals::new(FrameIntervalKind::Stepwise {
            min: interval(1, N),
            max: interval(1, 1),
            step: interval(N - 2, N - 1),
        })
        .unwrap();
        // Requested min rate N/1 (interval 1/N): denominator b*d*g = N*N*(N-2)
        // exceeds u64; numerator is zero, so the step check succeeds exactly.
        assert!(
            u128::from(N) * u128::from(N) * u128::from(N - 2) > u128::from(u64::MAX),
            "precondition: denominator product must exceed u64"
        );
        assert!(intervals.supports(rate(N, 1))); // exact min: offset zero
        // Interior interval (N-1)/N (rate N/(N-1)): numerator
        // ((N-1)*N - N)*(N-1) and denominator N*N*(N-2) both exceed u64,
        // and the period is off the (N-2)/(N-1) lattice.
        assert!(
            (u128::from(N - 1) * u128::from(N) - u128::from(N)) * u128::from(N - 1)
                > u128::from(u64::MAX),
            "precondition: numerator product must exceed u64"
        );
        assert!(!intervals.supports(rate(N, N - 1)));
    }

    #[test]
    fn below_min_rate_short_circuits_on_bounds() {
        let intervals = FrameIntervals::new(FrameIntervalKind::Stepwise {
            min: interval(1, 30),
            max: interval(1, 25),
            step: interval(1, 300),
        })
        .unwrap();
        // Interval 2/1 s exceeds the max interval 1/25 s: bounds fail first.
        assert!(!intervals.supports(rate(1, 2)));
    }

    #[test]
    fn interval_constructors_reject_empty_duplicates_and_inverted_ranges() {
        assert!(FrameIntervals::new(FrameIntervalKind::Discrete(Vec::new())).is_err());
        assert!(
            FrameIntervals::new(FrameIntervalKind::Discrete(vec![
                interval(1, 60),
                interval(2, 120),
            ]))
            .is_err()
        );
        assert!(
            FrameIntervals::new(FrameIntervalKind::Stepwise {
                min: interval(1, 25),
                max: interval(1, 60),
                step: interval(1, 1000),
            })
            .is_err()
        ); // min period > max period: inverted
        assert!(
            FrameIntervals::new(FrameIntervalKind::Continuous {
                min: interval(2, 60),
                max: interval(1, 60),
            })
            .is_err()
        );
    }

    // -- Size support -------------------------------------------------------

    #[test]
    fn size_membership_is_exact_per_kind() {
        let discrete = discrete_sizes(&[
            (640, 480, Descriptor::NotReported),
            (2560, 1440, Descriptor::NotReported),
        ]);
        assert!(discrete.contains(size(2560, 1440)));
        assert!(!discrete.contains(size(1920, 1080)));

        let stepwise = FrameSizes::new(FrameSizeKind::Stepwise {
            min: size(640, 480),
            max: size(1920, 1080),
            step_width: NonZeroU32::new(2).unwrap(),
            step_height: NonZeroU32::new(2).unwrap(),
        })
        .unwrap();
        assert!(stepwise.contains(size(642, 482)));
        assert!(stepwise.contains(size(1920, 1080))); // max edge inclusive
        assert!(!stepwise.contains(size(641, 482))); // odd width off lattice
        assert!(!stepwise.contains(size(640, 481))); // odd height off lattice
        assert!(!stepwise.contains(size(1922, 1080))); // above max

        let continuous = FrameSizes::new(FrameSizeKind::Continuous {
            min: size(640, 480),
            max: size(1920, 1080),
        })
        .unwrap();
        assert!(continuous.contains(size(1001, 999))); // any size inside
        assert!(!continuous.contains(size(639, 480)));
    }

    #[test]
    fn size_constructors_reject_empty_duplicates_and_bad_ranges() {
        assert!(FrameSizes::new(FrameSizeKind::Discrete(Vec::new())).is_err());
        assert!(
            FrameSizes::new(FrameSizeKind::Discrete(vec![
                DiscreteSize {
                    size: size(640, 480),
                    intervals: Descriptor::NotReported
                },
                DiscreteSize {
                    size: size(640, 480),
                    intervals: Descriptor::NotReported
                },
            ]))
            .is_err()
        );
        assert!(
            FrameSizes::new(FrameSizeKind::Stepwise {
                min: size(1920, 480),
                max: size(640, 1080),
                step_width: NonZeroU32::new(1).unwrap(),
                step_height: NonZeroU32::new(1).unwrap(),
            })
            .is_err()
        );
        assert!(
            FrameSizes::new(FrameSizeKind::Continuous {
                min: size(640, 1080),
                max: size(1920, 480),
            })
            .is_err()
        );
    }

    // -- NodeCapabilities ---------------------------------------------------

    fn nv12_yuyv_node(yuyv_intervals: Descriptor<FrameIntervals>) -> NodeCapabilities {
        NodeCapabilities::new(
            vec![
                format(CaptureBufferType::SinglePlanar, NV12),
                format(CaptureBufferType::SinglePlanar, YUYV),
            ],
            vec![
                caps_entry(
                    NV12,
                    Descriptor::Available(discrete_sizes(&[(
                        2560,
                        1440,
                        Descriptor::Available(discrete_intervals(&[
                            interval(1, 60),
                            interval(1, 50),
                            interval(1, 30),
                            interval(1, 25),
                        ])),
                    )])),
                ),
                caps_entry(
                    YUYV,
                    Descriptor::Available(discrete_sizes(&[(2560, 1440, yuyv_intervals)])),
                ),
            ],
        )
        .unwrap()
    }

    #[test]
    fn shadowcast_nv12_60fps_supported_and_yuyv_60fps_rejected() {
        let node = nv12_yuyv_node(Descriptor::Available(discrete_intervals(&[
            interval(1, 50),
            interval(1, 30),
            interval(1, 25),
        ])));
        let nv12_mode = CaptureMode {
            captured_fourcc: NV12,
            size: size(2560, 1440),
            rate: rate(60, 1),
        };
        let yuyv_mode = CaptureMode {
            captured_fourcc: YUYV,
            size: size(2560, 1440),
            rate: rate(60, 1),
        };
        assert_eq!(
            node.assess(CaptureBufferType::SinglePlanar, nv12_mode),
            SupportVerdict::Supported
        );
        assert_eq!(
            node.assess(CaptureBufferType::SinglePlanar, yuyv_mode),
            SupportVerdict::Unsupported(UnsupportedReason::FrameRate)
        );
        // No cross-format or cross-size borrowing: 30/1 is YUYV's rate, not NV12's
        // concern here, and NV12 at 1920x1080 is not a size at all.
        assert_eq!(
            node.assess(
                CaptureBufferType::SinglePlanar,
                CaptureMode {
                    captured_fourcc: NV12,
                    size: size(1920, 1080),
                    rate: rate(30, 1)
                }
            ),
            SupportVerdict::Unsupported(UnsupportedReason::FrameSize)
        );
    }

    #[test]
    fn fourcc_is_checked_per_buffer_type_before_sizes() {
        let node = nv12_yuyv_node(Descriptor::NotReported);
        // YUYV is announced only on single-planar: multi-planar is FourCc unsupported.
        assert_eq!(
            node.assess(
                CaptureBufferType::MultiPlanar,
                CaptureMode {
                    captured_fourcc: YUYV,
                    size: size(2560, 1440),
                    rate: rate(30, 1)
                }
            ),
            SupportVerdict::Unsupported(UnsupportedReason::FourCc)
        );
        assert_eq!(
            node.assess(
                CaptureBufferType::SinglePlanar,
                CaptureMode {
                    captured_fourcc: CapturedFourCc::from_bytes(*b"BGR3"),
                    size: size(2560, 1440),
                    rate: rate(60, 1)
                }
            ),
            SupportVerdict::Unsupported(UnsupportedReason::FourCc)
        );
    }

    #[test]
    fn unknown_states_are_distinct_from_unsupported() {
        let sizes_unreported = NodeCapabilities::new(
            vec![format(CaptureBufferType::SinglePlanar, NV12)],
            vec![caps_entry(NV12, Descriptor::NotReported)],
        )
        .unwrap();
        assert_eq!(
            sizes_unreported.assess(
                CaptureBufferType::SinglePlanar,
                CaptureMode {
                    captured_fourcc: NV12,
                    size: size(2560, 1440),
                    rate: rate(60, 1)
                }
            ),
            SupportVerdict::Unknown(UnknownReason::FrameSizesNotReported)
        );

        let intervals_unreported = NodeCapabilities::new(
            vec![format(CaptureBufferType::SinglePlanar, NV12)],
            vec![caps_entry(
                NV12,
                Descriptor::Available(discrete_sizes(&[(2560, 1440, Descriptor::NotReported)])),
            )],
        )
        .unwrap();
        assert_eq!(
            intervals_unreported.assess(
                CaptureBufferType::SinglePlanar,
                CaptureMode {
                    captured_fourcc: NV12,
                    size: size(2560, 1440),
                    rate: rate(60, 1)
                }
            ),
            SupportVerdict::Unknown(UnknownReason::FrameIntervalsNotReported)
        );
    }

    #[test]
    fn sizes_inside_a_native_range_require_an_exact_query() {
        let node = NodeCapabilities::new(
            vec![format(CaptureBufferType::SinglePlanar, NV12)],
            vec![caps_entry(
                NV12,
                Descriptor::Available(
                    FrameSizes::new(FrameSizeKind::Stepwise {
                        min: size(640, 480),
                        max: size(2560, 1440),
                        step_width: NonZeroU32::new(2).unwrap(),
                        step_height: NonZeroU32::new(2).unwrap(),
                    })
                    .unwrap(),
                ),
            )],
        )
        .unwrap();
        // Even a rate the min size would accept stays query-required at an inner
        // size: no extrapolation from range data.
        assert_eq!(
            node.assess(
                CaptureBufferType::SinglePlanar,
                CaptureMode {
                    captured_fourcc: NV12,
                    size: size(1280, 720),
                    rate: rate(60, 1)
                }
            ),
            SupportVerdict::NeedsExactIntervals
        );
        assert_eq!(
            node.assess(
                CaptureBufferType::SinglePlanar,
                CaptureMode {
                    captured_fourcc: NV12,
                    size: size(1281, 720),
                    rate: rate(60, 1)
                }
            ),
            SupportVerdict::Unsupported(UnsupportedReason::FrameSize)
        );
    }

    #[test]
    fn per_size_intervals_are_never_shared_between_sizes() {
        // 640x480 accepts 60/1; 2560x1440 accepts only 30/1. The inner size's own
        // exact data must refuse 60/1 even though another size accepts it.
        let node = NodeCapabilities::new(
            vec![format(CaptureBufferType::SinglePlanar, NV12)],
            vec![caps_entry(
                NV12,
                Descriptor::Available(discrete_sizes(&[
                    (
                        640,
                        480,
                        Descriptor::Available(discrete_intervals(&[interval(1, 60)])),
                    ),
                    (
                        2560,
                        1440,
                        Descriptor::Available(discrete_intervals(&[interval(1, 30)])),
                    ),
                ])),
            )],
        )
        .unwrap();
        assert_eq!(
            node.assess(
                CaptureBufferType::SinglePlanar,
                CaptureMode {
                    captured_fourcc: NV12,
                    size: size(640, 480),
                    rate: rate(60, 1)
                }
            ),
            SupportVerdict::Supported
        );
        assert_eq!(
            node.assess(
                CaptureBufferType::SinglePlanar,
                CaptureMode {
                    captured_fourcc: NV12,
                    size: size(2560, 1440),
                    rate: rate(60, 1)
                }
            ),
            SupportVerdict::Unsupported(UnsupportedReason::FrameRate)
        );
    }

    #[test]
    fn node_constructor_rejects_inconsistent_format_capability_sets() {
        let formats = || vec![format(CaptureBufferType::SinglePlanar, NV12)];
        // Orphan capability entry.
        assert!(
            NodeCapabilities::new(
                formats(),
                vec![
                    caps_entry(NV12, Descriptor::NotReported),
                    caps_entry(YUYV, Descriptor::NotReported),
                ],
            )
            .is_err()
        );
        // Missing capability entry.
        assert!(
            NodeCapabilities::new(
                vec![
                    format(CaptureBufferType::SinglePlanar, NV12),
                    format(CaptureBufferType::SinglePlanar, YUYV),
                ],
                vec![caps_entry(NV12, Descriptor::NotReported)],
            )
            .is_err()
        );
        // Duplicated capability entry.
        assert!(
            NodeCapabilities::new(
                formats(),
                vec![
                    caps_entry(NV12, Descriptor::NotReported),
                    caps_entry(NV12, Descriptor::NotReported),
                ],
            )
            .is_err()
        );
        // Empty format list.
        assert!(NodeCapabilities::new(Vec::new(), Vec::new()).is_err());
        // Duplicate format entry on the same buffer queue.
        assert!(
            NodeCapabilities::new(
                vec![
                    format(CaptureBufferType::SinglePlanar, NV12),
                    format(CaptureBufferType::SinglePlanar, NV12),
                ],
                vec![caps_entry(NV12, Descriptor::NotReported)],
            )
            .is_err()
        );
    }

    #[test]
    fn same_fourcc_on_two_buffer_types_shares_one_capability_entry() {
        let node = NodeCapabilities::new(
            vec![
                format(CaptureBufferType::SinglePlanar, NV12),
                format(CaptureBufferType::MultiPlanar, NV12),
            ],
            vec![caps_entry(NV12, Descriptor::NotReported)],
        )
        .unwrap();
        assert_eq!(node.fourcc_capabilities().len(), 1);
    }

    // -- Identity and topology ----------------------------------------------

    #[test]
    fn topology_and_identity_reject_invalid_data() {
        assert!(UsbTopology::new(String::new(), vec![NonZeroU8::new(1).unwrap()]).is_err());
        assert!(UsbTopology::new("   ".to_string(), vec![NonZeroU8::new(1).unwrap()]).is_err());
        assert!(
            UsbTopology::new("pci/0000:16".to_string(), vec![NonZeroU8::new(1).unwrap()]).is_err()
        );
        assert!(UsbTopology::new("pci-0000:16".to_string(), Vec::new()).is_err());
        assert_eq!(
            DeviceIdentity::new(
                0x32ed,
                0x3701,
                UsbTopology::new("c".to_string(), vec![NonZeroU8::new(1).unwrap()]).unwrap(),
                Some(String::new())
            )
            .unwrap_err(),
            CaptureDataError::InvalidIdentity { field: "serial" }
        );
    }

    #[test]
    fn identity_preserves_serial_characters_and_case() {
        let id = identity(Some(" KT044001 "), "pci-0000:16:00.0", &[1]);
        assert_eq!(id.serial(), Some(" KT044001 "));
    }

    #[test]
    fn unique_serial_resolves_to_original_index_ignoring_topology() {
        let requested = identity(Some("KT044001"), "pci-0000:16:00.0", &[1]);
        let mut observed = vec![
            identity(Some("OTHER1"), "pci-0000:16:00.0", &[2]),
            identity(None, "pci-0000:16:00.0", &[3]),
        ];
        // Same serial, moved to another port: still resolved.
        observed.push(identity(Some("KT044001"), "pci-0000:1b:00.0", &[7]));
        assert_eq!(resolve_identity(&requested, &observed), Ok(2));
    }

    #[test]
    fn zero_serial_matches_is_not_found_without_topology_fallback() {
        let requested = identity(Some("KT044001"), "pci-0000:16:00.0", &[1]);
        // A serial-less device on the exact requested topology must NOT satisfy
        // a serial request.
        let observed = vec![identity(None, "pci-0000:16:00.0", &[1])];
        assert_eq!(
            resolve_identity(&requested, &observed),
            Err(IdentityError::NotFound)
        );
    }

    #[test]
    fn duplicate_serials_are_ambiguous() {
        let requested = identity(Some("KT044001"), "pci-0000:16:00.0", &[1]);
        let observed = vec![
            identity(Some("KT044001"), "pci-0000:16:00.0", &[1]),
            identity(Some("KT044001"), "pci-0000:1b:00.0", &[2]),
        ];
        assert_eq!(
            resolve_identity(&requested, &observed),
            Err(IdentityError::DuplicateSerial { candidates: 2 })
        );
    }

    #[test]
    fn serialless_duplicates_are_ambiguous_even_on_distinct_ports() {
        let requested = identity(None, "pci-0000:16:00.0", &[1]);
        let observed = vec![
            identity(None, "pci-0000:16:00.0", &[1]),
            identity(None, "pci-0000:1b:00.0", &[2]),
        ];
        assert_eq!(
            resolve_identity(&requested, &observed),
            Err(IdentityError::SeriallessDuplicates { candidates: 2 })
        );
    }

    #[test]
    fn serialless_unique_candidate_must_keep_topology() {
        let requested = identity(None, "pci-0000:16:00.0", &[1]);
        let observed = vec![identity(None, "pci-0000:1b:00.0", &[7])];
        assert_eq!(
            resolve_identity(&requested, &observed),
            Err(IdentityError::NotFound)
        );
    }

    #[test]
    fn serialless_unique_same_port_resolves_to_original_index() {
        let requested = identity(None, "pci-0000:16:00.0", &[1, 3]);
        let observed = vec![
            identity(Some("OTHER1"), "pci-0000:16:00.0", &[1]),
            identity(None, "pci-0000:16:00.0", &[1, 3]),
        ];
        assert_eq!(resolve_identity(&requested, &observed), Ok(1));
    }

    #[test]
    fn serialless_no_candidates_is_not_found() {
        let requested = identity(None, "pci-0000:16:00.0", &[1]);
        // Only serial-bearing devices observed: no serial-less candidate.
        let observed = vec![identity(Some("KT044001"), "pci-0000:16:00.0", &[1])];
        assert_eq!(
            resolve_identity(&requested, &observed),
            Err(IdentityError::NotFound)
        );
    }

    #[test]
    fn vid_pid_differ_means_no_serialless_candidate() {
        let mut requested = identity(None, "pci-0000:16:00.0", &[1]);
        let mut same_but_other_pid = identity(None, "pci-0000:16:00.0", &[1]);
        same_but_other_pid.product_id = 0x3702;
        requested.product_id = 0x3701;
        let observed = vec![same_but_other_pid];
        assert_eq!(
            resolve_identity(&requested, &observed),
            Err(IdentityError::NotFound)
        );
    }

    // -- Serialization ------------------------------------------------------

    #[test]
    fn inspectable_data_serializes_losslessly() {
        let node = nv12_yuyv_node(Descriptor::NotReported);
        let json = serde_json::to_string(&node).unwrap();
        // FourCC keeps all 32 bits as the kernel value: lossless, no text loss.
        assert!(
            json.contains("842094158"),
            "NV12 kernel value 0x3231564E in: {json}"
        );
        assert!(json.contains("NotReported"), "{json}");
        // Non-ASCII / space bytes survive byte-for-byte.
        let exotic = CapturedFourCc::from_kernel(0x20FF_0180);
        let json = serde_json::to_string(&exotic).unwrap();
        assert_eq!(json, "553582976");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serde_json::json!(0x20FF_0180u64)
        );
        let identity = identity(Some("KT044001"), "pci-0000:16:00.0", &[1, 3]);
        let json = serde_json::to_string(&identity).unwrap();
        assert!(json.contains("KT044001"), "{json}");
        assert!(json.contains("vendor_id"), "{json}");
    }

    #[test]
    fn serial_on_unrelated_vid_pid_does_not_match() {
        let mut requested = identity(Some("KT044001"), "pci-0000:16:00.0", &[1]);
        requested.product_id = 0x3702;
        // Same serial, same topology, different product: not the requested device.
        let observed = vec![identity(Some("KT044001"), "pci-0000:16:00.0", &[1])];
        assert_eq!(
            resolve_identity(&requested, &observed),
            Err(IdentityError::NotFound)
        );
    }
}
