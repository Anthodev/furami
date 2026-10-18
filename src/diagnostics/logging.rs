//! One bounded producer path and one worker-owned storage/console path.
//!
//! Managed names are relative to an opened, no-follow directory. The private
//! directory and the permanent flock coordinate cooperating instances; no data
//! descriptor survives a lock transaction. This is not a power-loss audit log.
use super::report::{
    Observation, OsReport, ReportRevision, ReportSnapshot, SessionIdentity, UnavailableReason,
};
use parking_lot::Mutex;
use serde::{
    Deserialize, Serialize,
    ser::{SerializeMap, Serializer},
};
use std::{
    ffi::{CString, OsString},
    fs::{File, OpenOptions, Permissions, TryLockError},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{EnvFilter, Layer, layer::Context, prelude::*};

const MAX_FILES: usize = 5;
const MAX_FIELDS: usize = 32;
const TOKEN_BYTES: usize = 96;
const LOCK_NAME: &str = ".furami-log.lock";
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
const OS_RELEASE_BYTES: usize = 16_384;
const HEADER_BYTES: usize = 256;
const O_RDONLY: i32 = 0;
const O_RDWR: i32 = 2;
const O_CREAT: i32 = 0o100;
const O_EXCL: i32 = 0o200;
const O_NONBLOCK: i32 = 0o4000;
const O_DIRECTORY: i32 = 0o200000;
const O_NOFOLLOW: i32 = 0o400000;
const O_CLOEXEC: i32 = 0o2000000;
// Linux only, matching the application's supported platform. Minimal C ABI
// bindings keep all managed operations dirfd-relative without a new crate.
unsafe extern "C" {
    #[link_name = "openat"]
    fn sys_openat(dirfd: i32, path: *const std::ffi::c_char, flags: i32, mode: u32) -> i32;
    #[link_name = "mkdirat"]
    fn sys_mkdirat(dirfd: i32, path: *const std::ffi::c_char, mode: u32) -> i32;
    #[link_name = "renameat"]
    fn sys_renameat(
        oldfd: i32,
        old: *const std::ffi::c_char,
        newfd: i32,
        new: *const std::ffi::c_char,
    ) -> i32;
    #[link_name = "unlinkat"]
    fn sys_unlinkat(dirfd: i32, path: *const std::ffi::c_char, flags: i32) -> i32;
    #[link_name = "geteuid"]
    fn sys_geteuid() -> u32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct LogLimits {
    pub file_bytes: u64,
    pub files: usize,
    pub event_bytes: usize,
    pub report_bytes: usize,
    pub queue_messages: usize,
}
impl Default for LogLimits {
    fn default() -> Self {
        Self {
            file_bytes: 1_048_576,
            files: 5,
            event_bytes: 8_192,
            report_bytes: 131_072,
            queue_messages: 64,
        }
    }
}
impl LogLimits {
    fn valid(self) -> bool {
        let max = Self::default();
        self.file_bytes >= 512
            && self.file_bytes <= max.file_bytes
            && (1..=MAX_FILES).contains(&self.files)
            && (128..=max.event_bytes).contains(&self.event_bytes)
            && (256..=max.report_bytes).contains(&self.report_bytes)
            && (1..=max.queue_messages).contains(&self.queue_messages)
    }
}

#[derive(Clone, Debug)]
pub struct DiagnosticsConfig {
    pub xdg_state_home: Option<OsString>,
    pub home: Option<OsString>,
    pub limits: LogLimits,
    pub logging_enabled: bool,
}
impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            xdg_state_home: std::env::var_os("XDG_STATE_HOME"),
            home: std::env::var_os("HOME"),
            limits: LogLimits::default(),
            logging_enabled: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum SinkStatus {
    Starting,
    Ready,
    Unavailable,
    Stopped,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum LogOrigin {
    XdgStateHome,
    HomeFallback,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LogLocation {
    pub path: String,
    pub origin: LogOrigin,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum DiagnosticsCode {
    InvalidStateRoot,
    MissingHome,
    PathEncoding,
    InvalidLogFilter,
    SubscriberUnavailable,
    DirectoryAccess,
    UnsafeEntry,
    LockContended,
    LockUnsupported,
    WriteFailed,
    RotationFailed,
    QueueFull,
    OversizedRecord,
    OversizedReport,
    WorkerStopped,
    ConsoleWriteFailed,
    OpenRejected,
    LauncherFailed,
    ShutdownIncomplete,
    CounterExhausted,
    InvalidReportRevision,
    InvalidLimits,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum DiagnosticsOperation {
    Resolve,
    Configure,
    Subscribe,
    Directory,
    Validate,
    Lock,
    Repair,
    Write,
    Rollback,
    Rotate,
    Enqueue,
    Encode,
    Console,
    OpenFolder,
    Shutdown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum IoKind {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    InvalidInput,
    InvalidData,
    WouldBlock,
    Interrupted,
    WriteZero,
    Unsupported,
    BrokenPipe,
    StorageFull,
    Other,
}
impl From<io::ErrorKind> for IoKind {
    fn from(value: io::ErrorKind) -> Self {
        match value {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            io::ErrorKind::InvalidInput => Self::InvalidInput,
            io::ErrorKind::InvalidData => Self::InvalidData,
            io::ErrorKind::WouldBlock => Self::WouldBlock,
            io::ErrorKind::Interrupted => Self::Interrupted,
            io::ErrorKind::WriteZero => Self::WriteZero,
            io::ErrorKind::Unsupported => Self::Unsupported,
            io::ErrorKind::BrokenPipe => Self::BrokenPipe,
            io::ErrorKind::StorageFull => Self::StorageFull,
            _ => Self::Other,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticsError {
    pub operation: DiagnosticsOperation,
    pub code: DiagnosticsCode,
    pub io_kind: Option<IoKind>,
    pub os_code: Option<i32>,
}
impl DiagnosticsError {
    fn new(operation: DiagnosticsOperation, code: DiagnosticsCode) -> Self {
        Self {
            operation,
            code,
            io_kind: None,
            os_code: None,
        }
    }
    fn io(operation: DiagnosticsOperation, code: DiagnosticsCode, error: &io::Error) -> Self {
        Self {
            operation,
            code,
            io_kind: Some(error.kind().into()),
            os_code: error.raw_os_error(),
        }
    }
    pub fn message(self) -> &'static str {
        match self.code {
            DiagnosticsCode::InvalidStateRoot
            | DiagnosticsCode::MissingHome
            | DiagnosticsCode::PathEncoding
            | DiagnosticsCode::DirectoryAccess
            | DiagnosticsCode::UnsafeEntry => "The log location is inaccessible.",
            DiagnosticsCode::OpenRejected => "Opening the log folder was refused.",
            DiagnosticsCode::LauncherFailed => "The log folder launcher failed after starting.",
            DiagnosticsCode::QueueFull
            | DiagnosticsCode::OversizedRecord
            | DiagnosticsCode::OversizedReport
            | DiagnosticsCode::ShutdownIncomplete => {
                "Logs are incomplete: some diagnostics were lost."
            }
            DiagnosticsCode::InvalidLogFilter | DiagnosticsCode::SubscriberUnavailable => {
                "Logging is partially unavailable."
            }
            _ => "Writing logs is impossible.",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct FolderRequestId(pub u64);
impl FolderRequestId {
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum FolderState {
    #[default]
    Idle,
    Preparing,
    OpenRequested,
    Failed,
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct FolderHealth {
    pub request: Option<FolderRequestId>,
    pub state: FolderState,
    pub error: Option<DiagnosticsError>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticsHealth {
    pub logging_enabled: bool,
    pub sink_status: SinkStatus,
    pub location: Option<LogLocation>,
    pub limits: LogLimits,
    pub last_error: Option<DiagnosticsError>,
    /// Sticky storage failure; later queue/console faults cannot erase its cause.
    pub sink_error: Option<DiagnosticsError>,
    pub rollback_error: Option<DiagnosticsError>,
    pub dropped_events: u64,
    pub oversized_records: u64,
    pub oversized_reports: u64,
    pub folder: FolderHealth,
}
impl DiagnosticsHealth {
    pub fn starting(limits: LogLimits) -> Self {
        Self {
            logging_enabled: false,
            sink_status: SinkStatus::Starting,
            location: None,
            limits,
            last_error: None,
            sink_error: None,
            rollback_error: None,
            dropped_events: 0,
            oversized_records: 0,
            oversized_reports: 0,
            folder: FolderHealth::default(),
        }
    }
    pub fn has_problem(&self) -> bool {
        self.logging_enabled
            && (self.last_error.is_some()
                || self.sink_error.is_some()
                || self.rollback_error.is_some()
                || self.folder.error.is_some()
                || self.dropped_events != 0
                || self.oversized_records != 0
                || self.oversized_reports != 0
                || self.sink_status == SinkStatus::Unavailable)
    }
    pub fn message(&self) -> &'static str {
        if !self.logging_enabled {
            return "";
        }
        self.folder
            .error
            .or(self.sink_error)
            .or(self.last_error)
            .map(DiagnosticsError::message)
            .unwrap_or_else(|| {
                if self.has_problem() {
                    "Logs are incomplete: some diagnostics were lost."
                } else {
                    ""
                }
            })
    }
}
#[derive(Clone, Debug)]
pub struct FolderPreparation {
    pub request: FolderRequestId,
    pub result: Result<String, DiagnosticsError>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub enum EventLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}
impl EventLevel {
    fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}
#[derive(Clone, Copy)]
struct Token {
    bytes: [u8; TOKEN_BYTES],
    len: u8,
}
impl Token {
    fn new(value: &str) -> Option<Self> {
        if value.is_empty()
            || value.len() > TOKEN_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b':' | b'.')
            })
        {
            return None;
        }
        let mut token = Self {
            bytes: [0; TOKEN_BYTES],
            len: value.len() as u8,
        };
        token.bytes[..value.len()].copy_from_slice(value.as_bytes());
        Some(token)
    }
    fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len as usize]).expect("token is validated ASCII")
    }
}
impl Serialize for Token {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.text())
    }
}
#[derive(Clone, Copy, Serialize)]
#[serde(untagged)]
enum Scalar {
    Unsigned(u64),
    Signed(i64),
    Float(f64),
    Bool(bool),
    Token(Token),
    Omitted,
}
/// Scalar projections only. Unknown field names and invalid tokens are omitted;
/// neither formatting arbitrary values nor accepting arbitrary JSON is exposed.
#[derive(Clone, Copy)]
pub struct SafeField {
    name: &'static str,
    value: Scalar,
}
impl SafeField {
    pub fn u64(name: &'static str, value: u64) -> Self {
        Self {
            name,
            value: Scalar::Unsigned(value),
        }
    }
    pub fn i64(name: &'static str, value: i64) -> Self {
        Self {
            name,
            value: Scalar::Signed(value),
        }
    }
    pub fn f64(name: &'static str, value: f64) -> Self {
        Self {
            name,
            value: if value.is_finite() {
                Scalar::Float(value)
            } else {
                Scalar::Omitted
            },
        }
    }
    pub fn bool(name: &'static str, value: bool) -> Self {
        Self {
            name,
            value: Scalar::Bool(value),
        }
    }
    pub fn token(name: &'static str, value: &str) -> Self {
        Self {
            name,
            value: Token::new(value)
                .map(Scalar::Token)
                .unwrap_or(Scalar::Omitted),
        }
    }
    fn permitted(self) -> bool {
        match self.value {
            Scalar::Omitted => false,
            Scalar::Token(value) => token_name(self.name) && token_allowed(self.name, value.text()),
            _ => numeric_name(self.name),
        }
    }
}

#[cfg(test)]
mod startup_option_tests {
    use super::*;

    #[test]
    fn force_window_is_a_finite_property_token_not_an_arbitrary_value() {
        assert!(SafeField::token("property", "force-window").permitted());
        for value in [
            "force-window=yes",
            "force-window-custom",
            "/private/path",
            "no",
        ] {
            assert!(!SafeField::token("property", value).permitted());
        }
        assert!(!SafeField::token("arbitrary", "force-window").permitted());
    }
    #[test]
    fn qualification_telemetry_preserves_finite_privacy_contract() {
        assert_eq!(
            known_code("qualification_state"),
            Some("qualification_state")
        );
        for phase in [
            "Idle",
            "WaitingSurface",
            "Opening",
            "Ready",
            "Stopping",
            "Releasing",
            "QuitReady",
        ] {
            assert!(SafeField::token("phase", phase).permitted());
        }
        assert!(SafeField::bool("gpu_next", true).permitted());
        assert!(SafeField::bool("x11vk", true).permitted());
        assert!(!SafeField::token("phase", "/private/fixture.y4m").permitted());
        assert!(!SafeField::token("snapshot", "private data").permitted());
        assert!(!SafeField::token("backend", "arbitrary backend name").permitted());
    }
}
fn token_name(name: &str) -> bool {
    matches!(
        name,
        "phase"
            | "cleanup"
            | "stage"
            | "cause"
            | "kind"
            | "severity"
            | "operation"
            | "purpose"
            | "pass"
            | "reason"
            | "status"
            | "source"
            | "property"
            | "io_kind"
            | "code"
            | "target"
            | "control"
            | "object_name"
            | "format"
            | "format_policy"
            | "fourcc"
    )
}
fn token_allowed(name: &str, value: &str) -> bool {
    match name {
        "control" | "object_name" => known_control(value),
        "cleanup" => matches!(value, "Complete" | "Draining" | "Blocked"),
        "phase" => matches!(
            value,
            "Stopped"
                | "Active"
                | "ApplyingTreatments"
                | "RestoringTreatments"
                | "PausePending"
                | "Paused"
                | "Validating"
                | "ClosingOld"
                | "OpeningCandidate"
                | "CleaningFailedCandidate"
                | "ValidatingPrior"
                | "OpeningRestore"
                | "CleaningFailedRestore"
                | "ValidatingResume"
                | "ClosingResume"
                | "OpeningResume"
                | "CleaningFailedResume"
                | "ErrorWithActiveRestored"
                | "ErrorWithoutActive"
                | "Disconnected"
                | "Recovering"
                | "SelectionRequired"
                | "Stopping"
                | "ShutdownReady"
                | "Snapshot"
                | "Write"
                | "Set"
                | "Readback"
                | "Verify"
                | "Live"
                | "Running"
                | "Idle"
                | "WaitingSurface"
                | "Opening"
                | "Ready"
                | "Releasing"
                | "Failed"
                | "QuitReady"
        ),
        "stage" => matches!(
            value,
            "Prevalidation"
                | "InputConstruction"
                | "Open"
                | "Negotiation"
                | "StreamStart"
                | "Verification"
                | "Unknown"
                | "Discovery"
                | "Arguments"
                | "NativeStartup"
                | "MediaStartup"
                | "Reexec"
        ),
        "cause" => matches!(
            value,
            "RequestedModeRefused" | "Busy" | "Permission" | "Generic"
        ),
        "severity" => matches!(
            value,
            "fatal"
                | "error"
                | "warn"
                | "info"
                | "debug"
                | "trace"
                | "unknown"
                | "Debug"
                | "Warning"
                | "Critical"
                | "Fatal"
                | "Info"
                | "Unknown"
        ),
        "purpose" => matches!(
            value,
            "Candidate" | "Restore" | "Reconnect" | "Resume" | "Recovery" | "Unknown"
        ),
        "pass" => matches!(value, "Candidate" | "Restore" | "Open" | "None"),
        "status" => matches!(
            value,
            "Validated"
                | "SelectionRequired"
                | "Failed"
                | "Configured"
                | "Approximate"
                | "ObservedCompatible"
                | "Unverified"
        ),
        "reason" => matches!(
            value,
            "Eof"
                | "Stop"
                | "Quit"
                | "Error"
                | "Redirect"
                | "Deadline"
                | "EvidenceLost"
                | "BackendUnavailable"
                | "Unknown"
                | "deadline"
                | "root-destroyed"
                | "not-visible"
                | "application-close"
                | "superseded"
                | "native-state-superseded"
        ),
        "kind" => matches!(
            value,
            "TreatmentResult"
                | "TreatmentFault"
                | "OpenVerified"
                | "OpenFailed"
                | "SessionFailed"
                | "StreamEnded"
                | "OwnerStopped"
                | "NativeReleased"
                | "CleanupBlocked"
                | "AudioAvailability"
                | "AudioDetached"
                | "PauseObserved"
                | "Command"
                | "Property"
                | "SetProperty"
                | "PwMetadataExit"
                | "Prevalidation"
                | "CatalogUnavailable"
                | "CommandSubmission"
                | "CommandRejected"
                | "Unconfirmed"
                | "Snapshot"
                | "DraftVideo"
                | "DraftIdentity"
                | "DraftAudio"
                | "DraftSource"
                | "DraftRenderer"
                | "Open"
                | "Apply"
                | "Restart"
                | "Reconnect"
                | "Choose"
                | "Volume"
                | "Mute"
                | "Pause"
                | "Close"
                | "Quit"
                | "Settings"
                | "ApplicationClose"
                | "SaveRetry"
                | "CloseWithoutSave"
                | "SettingsReset"
                | "ResetCancel"
                | "ResetConfirm"
                | "PreferenceVolume"
                | "PreferenceMute"
                | "Unknown"
        ),
        "property" => matches!(
            value,
            "scale"
                | "cscale"
                | "dscale"
                | "scale-antiring"
                | "dither"
                | "deband"
                | "deband-iterations"
                | "deband-grain"
                | "interpolation"
                | "config"
                | "load-scripts"
                | "input-default-bindings"
                | "input-vo-keyboard"
                | "input-terminal"
                | "input-cursor"
                | "input-cursor-passthrough"
                | "cursor-autohide"
                | "force-window"
                | "vo"
                | "gpu-api"
                | "gpu-context"
                | "gpu-sw"
                | "untimed"
                | "correct-pts"
                | "video-latency-hacks"
                | "cache"
                | "demuxer-readahead-secs"
                | "container-fps-override"
                | "pause"
                | "demuxer-lavf-format"
                | "demuxer-lavf-o"
                | "audio"
                | "wid"
                | "terminal"
                | "demuxer"
                | "loop-file"
                | "sub-auto"
                | "audio-file-auto"
                | "Unknown"
        ),
        "code" => matches!(
            value,
            "CaptureDiscovery"
                | "CaptureSelection"
                | "CaptureArguments"
                | "AudioUnavailable"
                | "NoDisplay"
                | "InvalidDisplayEnvironment"
                | "MediaPrefixEnvironment"
                | "InvalidMediaPrefix"
                | "NativeLaunch"
                | "NoFallbackExec"
                | "qualification_arguments"
                | "qualification_path"
                | "qualification_format"
                | "qualification_renderer"
                | "qt_plugin_missing"
                | "qt_plugin_invalid"
                | "qt_plugin_load"
                | "qualification"
        ),
        "format_policy" => matches!(value, "Auto" | "Manual"),
        "fourcc" | "format" => matches!(value, "YUYV" | "NV12" | "MJPG" | "BGR3" | "RGB3" | "UYVY"),
        "target" => matches!(
            value,
            "furami"
                | "furami::app::settings"
                | "furami::capture::apply"
                | "furami::media::controller"
                | "furami::media::ffi"
                | "furami::media::gate"
                | "furami::diagnostics"
                | "furami::diagnostics::logging_tests"
                | "furami::ui::runtime"
        ),
        "source" => matches!(
            value,
            "host" | "root" | "chrome" | "host_input_method" | "host_key" | "host_key_release"
        ),
        // Paths, native prose and lexical ASCII are not redaction mechanisms.
        _ => false,
    }
}
fn numeric_name(name: &str) -> bool {
    if name == "fourcc" {
        return true;
    }
    if matches!(
        name,
        "settings_refused"
            | "reset_token"
            | "stats_hidden"
            | "closing"
            | "quit"
            | "can_apply"
            | "can_reconnect"
            | "shutdown_ready"
            | "root"
            | "host"
            | "root_width"
            | "root_height"
            | "video_x"
            | "video_y"
            | "video_width"
            | "video_height"
            | "dpr"
            | "global_x"
            | "global_y"
            | "native_parent_null"
            | "transient_root"
            | "poisoned"
            | "authorized"
            | "button"
            | "backward"
            | "open"
            | "watch"
            | "epoch"
            | "candidate"
            | "volume_percent"
            | "muted"
            | "effective"
            | "transition"
            | "requested"
            | "observed"
            | "retained_preference"
            | "focus_restore_pending"
            | "fullscreen"
            | "native_previously_focused"
            | "repeat"
            | "text_entry"
            | "popup"
            | "event_type"
            | "matched"
            | "gate_phase"
            | "restart_generation"
            | "failed"
            | "can_restart"
            | "can_toggle_pause"
            | "can_set_gain"
            | "current"
            | "x11_parent"
            | "controlled"
            | "parent_preserved"
            | "shape_major"
            | "shape_minor"
            | "exit_code"
            | "owner_cleanup_authorized"
            | "gpu_next"
            | "x11vk"
    ) {
        return true;
    }
    matches!(
        name,
        "apply_id"
            | "attempt_id"
            | "generation"
            | "cleanup"
            | "cleanup_id"
            | "draft_revision"
            | "revision"
            | "request_id"
            | "sequence"
            | "ordinal"
            | "enabled"
            | "success"
            | "confirmed"
            | "truncated"
            | "mpv_error"
            | "errno"
            | "os_code"
            | "width"
            | "height"
            | "vendor_id"
            | "product_id"
            | "rate_numerator"
            | "rate_denominator"
            | "x"
            | "y"
            | "z"
            | "visible"
            | "active"
            | "mapped"
            | "surface"
            | "native_id"
            | "window_id"
            | "parent_id"
            | "count"
            | "scale"
            | "device_pixel_ratio"
            | "actual_x"
            | "actual_y"
            | "actual_width"
            | "actual_height"
            | "logical_x"
            | "logical_y"
            | "logical_width"
            | "logical_height"
            | "accepted"
            | "selected"
            | "paused"
            | "restored"
            | "pending"
            | "admitted"
            | "observation_epoch"
            | "client_api"
            | "lost"
            | "configured"
            | "available"
            | "elapsed_ms"
            | "position"
            | "from"
            | "to"
    )
}
fn known_code(code: &str) -> Option<&'static str> {
    const CODES: &[&str] = &[
        "application_starting",
        "application_exited",
        "startup_failed",
        "startup_audio_catalog_unavailable",
        "capture_requested",
        "capture_observed",
        "apply_validation",
        "apply_runtime",
        "runtime_snapshot",
        "settings_runtime",
        "settings_loaded",
        "settings_load_failed",
        "settings_write_failed",
        "settings_saved",
        "settings_unavailable",
        "session_event",
        "media_session_event",
        "media_event",
        "libmpv_loaded",
        "mpv_option",
        "mpv_log",
        "mpv_reply",
        "mpv_property_reply",
        "mpv_command_reply",
        "mpv_option_failed",
        "mpv_property_failed",
        "mpv_command_failed",
        "record_rejected",
        "qt_message",
        "native_payload_rejected",
        "ui_control",
        "qt_geometry",
        "qt_chrome_geometry",
        "native_startup",
        "native_shutdown",
        "native_environment",
        "native_surface",
        "native_release",
        "ui_intent",
        "log_folder_requested",
        "log_folder_result",
        "diagnostics_backend",
        "audio_route",
        "audio_helper",
        "output_catalog_unavailable",
        "qualification_command_rejected",
        "qualification_request_received",
        "qualification_request_parsed",
        "qualification_state",
        "qualification_backend",
        "qualification_volume",
        "qualification_mute",
        "reconnect_admitted",
        "apply_native_released",
        "apply_open_submit",
        "apply_owner_stopped",
        "owner_destroy_begin",
        "owner_destroy_complete",
        "mpv_end_file",
        "mpv_pause_observed",
        "mpv_initialized",
        "mpv_loadfile_submit",
        "treatment_read_invalid",
        "renderer_get_submit",
        "renderer_set_submit",
        "treatment_admitted",
        "treatment_confirmed",
        "renderer_owned_reply",
    ];
    CODES
        .iter()
        .copied()
        .find(|candidate| *candidate == code)
        .or_else(|| {
            NATIVE_SCHEMAS
                .iter()
                .find(|(candidate, _)| *candidate == code)
                .map(|(code, _)| *code)
        })
}
struct FieldSlice<'a>(&'a [SafeField]);
impl Serialize for FieldSlice<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for field in self
            .0
            .iter()
            .take(MAX_FIELDS)
            .filter(|field| field.permitted())
        {
            map.serialize_entry(field.name, &field.value)?;
        }
        map.end()
    }
}
#[derive(Serialize)]
struct EventRecord<'a> {
    schema: u8,
    instance_id: &'a str,
    event_sequence: u64,
    report_revision: ReportRevision,
    level: &'static str,
    event_code: &'static str,
    session_identity: SessionIdentity,
    fields: FieldSlice<'a>,
}
#[derive(Serialize)]
struct ReportRecord<'a, T> {
    schema: u8,
    record_type: &'static str,
    instance_id: &'a str,
    report_revision: ReportRevision,
    report: &'a T,
}
struct CappedWriter<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
    overflow: bool,
}
impl Write for CappedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.overflow = true;
            return Err(io::Error::from(io::ErrorKind::FileTooLarge));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode<T: Serialize>(value: &T, bytes: &mut Vec<u8>, limit: usize) -> bool {
    bytes.clear();
    let mut writer = CappedWriter {
        bytes,
        limit,
        overflow: false,
    };
    let result = serde_json::to_writer(&mut writer, value)
        .and_then(|()| writer.write_all(b"\n").map_err(serde_json::Error::io));
    result.is_ok() && !writer.overflow
}
struct PreparedReport {
    revision: ReportRevision,
    identity: SessionIdentity,
    bytes: Vec<u8>,
}
struct EncodedEvent {
    report: Arc<PreparedReport>,
    bytes: Vec<u8>,
}
enum Message {
    Report {
        epoch: u64,
        report: Arc<PreparedReport>,
    },
    Event {
        epoch: u64,
        event: EncodedEvent,
    },
}
struct FolderSlots {
    request: Option<(u64, FolderRequestId)>,
    response: Option<(u64, FolderPreparation)>,
    dispatchable: Option<(u64, FolderRequestId)>,
    awaiting_ack: Option<(u64, FolderRequestId)>,
    observing: Option<(u64, FolderRequestId)>,
}
/// Only the synchronous mutations made by one publication, never post-call health.
#[derive(Default)]
pub(crate) struct PublicationEffects {
    oversized_reports_added: u64,
    last_error: Option<ErrorAssignments>,
}
#[derive(Clone, Copy)]
struct ErrorAssignments {
    before: Option<DiagnosticsError>,
    after: DiagnosticsError,
    interleaved: bool,
}
impl PublicationEffects {
    fn assign_error(&mut self, health: &mut DiagnosticsHealth, error: DiagnosticsError) {
        match &mut self.last_error {
            Some(assignments) => {
                assignments.interleaved |= health.last_error != Some(assignments.after);
                assignments.after = error;
            }
            None => {
                self.last_error = Some(ErrorAssignments {
                    before: health.last_error,
                    after: error,
                    interleaved: false,
                })
            }
        }
        health.last_error = Some(error);
    }
    fn error(&mut self, shared: &Shared, error: DiagnosticsError) {
        self.assign_error(&mut shared.health.lock(), error);
    }
    fn oversized_report(&mut self, shared: &Shared) {
        let mut health = shared.health.lock();
        let previous = health.oversized_reports;
        health.oversized_reports = previous.saturating_add(1);
        self.oversized_reports_added += health.oversized_reports - previous;
        self.assign_error(
            &mut health,
            DiagnosticsError::new(
                DiagnosticsOperation::Encode,
                DiagnosticsCode::OversizedReport,
            ),
        );
    }
    /// Predict own feedback while leaving independently mutable fields untouched.
    /// True preserves an overwritten independent error as next-poll dirtiness.
    pub(crate) fn fold_into(self, baseline: &mut DiagnosticsHealth) -> bool {
        baseline.oversized_reports = baseline
            .oversized_reports
            .saturating_add(self.oversized_reports_added);
        match self.last_error {
            Some(assignments) => {
                let independent =
                    assignments.interleaved || assignments.before != baseline.last_error;
                baseline.last_error = Some(assignments.after);
                independent
            }
            None => false,
        }
    }
}
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum PublicationStage {
    BeforeOversize,
    AfterOversize,
    BeforeAdmission,
}
#[cfg(test)]
struct PublicationPause {
    stage: PublicationStage,
    reached: mpsc::Sender<()>,
    resume: Receiver<()>,
}
struct Admission {
    last_logging_epoch: u64,
}
struct Shared {
    health: Mutex<DiagnosticsHealth>,
    report: Mutex<Arc<PreparedReport>>,
    latest_report: Mutex<Option<(u64, Arc<PreparedReport>)>>,
    folders: Mutex<FolderSlots>,
    admission: Mutex<Admission>,
    accepting: AtomicBool,
    recording_epoch: AtomicU64,
    finished: AtomicBool,
    sequence: AtomicU64,
    last_report_revision: AtomicU64,
    revision_exhausted: AtomicBool,
    worker_thread: OnceLock<thread::Thread>,
    os: Mutex<Observation<OsReport>>,
    pool: Mutex<Vec<Vec<u8>>>,
    instance_id: String,
    limits: LogLimits,
    #[cfg(test)]
    publication_pause: Mutex<Option<PublicationPause>>,
}
impl Shared {
    fn error(&self, error: DiagnosticsError) {
        self.health.lock().last_error = Some(error);
    }
    fn storage_error(&self, error: DiagnosticsError) {
        let mut health = self.health.lock();
        health.sink_status = SinkStatus::Unavailable;
        health.last_error = Some(error);
        health.sink_error = Some(error);
    }
    fn wake(&self) {
        if let Some(worker) = self.worker_thread.get() {
            worker.unpark();
        }
    }
    fn buffer(&self) -> Vec<u8> {
        self.pool.lock().pop().unwrap_or_else(|| {
            Vec::with_capacity(
                self.limits
                    .event_bytes
                    .min(LogLimits::default().event_bytes),
            )
        })
    }
    fn recycle(&self, mut bytes: Vec<u8>) {
        bytes.clear();
        let mut pool = self.pool.lock();
        if pool.len() < self.limits.queue_messages.saturating_add(2).min(66) {
            pool.push(bytes);
        }
    }
    fn dropped(&self, code: DiagnosticsCode) {
        let mut health = self.health.lock();
        health.dropped_events = health.dropped_events.saturating_add(1);
        health.last_error = Some(DiagnosticsError::new(DiagnosticsOperation::Enqueue, code));
    }
    /// Claim one bounded sink effect, never retaining control locks across I/O.
    /// Terminal shutdown still drains work admitted in the current epoch.
    fn claim_effect(&self, epoch: u64) -> bool {
        let _admission = self.admission.lock();
        epoch != 0 && self.recording_epoch.load(Ordering::Acquire) == epoch
    }
}
#[derive(Clone)]
pub struct DiagnosticsHandle {
    shared: Arc<Shared>,
    sender: SyncSender<Message>,
}
impl DiagnosticsHandle {
    pub fn health(&self) -> DiagnosticsHealth {
        self.shared.health.lock().clone()
    }
    /// Change recording admission without waiting for the worker or touching disk.
    pub fn set_logging_enabled(&self, enabled: bool) -> Result<(), DiagnosticsError> {
        let mut admission = self.shared.admission.lock();
        let current = self.shared.recording_epoch.load(Ordering::Acquire);
        if !enabled {
            self.shared.recording_epoch.store(0, Ordering::Release);
            *self.shared.latest_report.lock() = None;
            let mut folders = self.shared.folders.lock();
            folders.request = None;
            folders.response = None;
            folders.dispatchable = None;
            folders.awaiting_ack = None;
            folders.observing = None;
            let mut health = self.shared.health.lock();
            health.logging_enabled = false;
            if health.folder.state == FolderState::Preparing {
                health.folder.state = FolderState::Idle;
            }
            drop(health);
            drop(folders);
            self.shared.wake();
            return Ok(());
        }
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Configure,
                DiagnosticsCode::WorkerStopped,
            ));
        }
        if current != 0 {
            return Ok(());
        }
        let epoch = admission.last_logging_epoch.checked_add(1).ok_or_else(|| {
            DiagnosticsError::new(
                DiagnosticsOperation::Configure,
                DiagnosticsCode::CounterExhausted,
            )
        })?;
        admission.last_logging_epoch = epoch;
        self.shared.recording_epoch.store(epoch, Ordering::Release);
        self.shared.health.lock().logging_enabled = true;
        let report = Arc::clone(&self.shared.report.lock());
        if !report.bytes.is_empty() {
            *self.shared.latest_report.lock() = Some((epoch, report));
        }
        self.shared.wake();
        Ok(())
    }
    pub fn os_report(&self) -> Observation<OsReport> {
        self.shared.os.lock().clone()
    }
    /// Compare borrowed observations; unchanged polling does not copy strings.
    pub(crate) fn environment_if_changed(
        &self,
        last_os: &Observation<OsReport>,
        last_health: &DiagnosticsHealth,
    ) -> Option<(Observation<OsReport>, DiagnosticsHealth)> {
        // No writer nests these locks. Keep this snapshot's order OS -> health;
        // worker OS initialization never acquires health while holding OS.
        let os = self.shared.os.lock();
        let health = self.shared.health.lock();
        if *os == *last_os && *health == *last_health {
            None
        } else {
            Some((os.clone(), health.clone()))
        }
    }
    /// Reserve a process-instance-wide report key across every handle clone.
    /// Gaps are valid; callers must never synthesize or locally increment keys.
    pub fn reserve_report_revision(&self) -> Option<ReportRevision> {
        if self.shared.revision_exhausted.load(Ordering::Acquire) {
            self.shared.error(DiagnosticsError::new(
                DiagnosticsOperation::Enqueue,
                DiagnosticsCode::CounterExhausted,
            ));
            return None;
        }
        match self.shared.last_report_revision.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |revision| revision.checked_add(1),
        ) {
            Ok(previous) => Some(ReportRevision(previous + 1)),
            Err(_) => {
                self.counter_exhausted();
                None
            }
        }
    }
    pub(crate) fn report_byte_limit(&self) -> usize {
        self.shared.limits.report_bytes
    }
    pub fn publish_report(&self, report: ReportSnapshot) {
        let _ = self.publish_report_with_effects(report);
    }
    pub(crate) fn publish_report_with_effects(&self, report: ReportSnapshot) -> PublicationEffects {
        let mut effects = PublicationEffects::default();
        if !self.shared.accepting.load(Ordering::Acquire) {
            effects.error(
                &self.shared,
                DiagnosticsError::new(
                    DiagnosticsOperation::Enqueue,
                    DiagnosticsCode::WorkerStopped,
                ),
            );
            return effects;
        }
        if self.shared.revision_exhausted.load(Ordering::Acquire) {
            effects.error(
                &self.shared,
                DiagnosticsError::new(
                    DiagnosticsOperation::Enqueue,
                    DiagnosticsCode::CounterExhausted,
                ),
            );
            return effects;
        }
        if report.revision.0 == 0
            || report.revision.0 > self.shared.last_report_revision.load(Ordering::Acquire)
        {
            effects.error(
                &self.shared,
                DiagnosticsError::new(
                    DiagnosticsOperation::Enqueue,
                    DiagnosticsCode::InvalidReportRevision,
                ),
            );
            return effects;
        }
        let encoding_epoch = self.shared.recording_epoch.load(Ordering::Acquire);
        let mut oversize_accounted = false;
        let mut bytes = Vec::with_capacity(self.shared.limits.report_bytes.min(16_384));
        let envelope = ReportRecord {
            schema: 1,
            record_type: "report",
            instance_id: &self.shared.instance_id,
            report_revision: report.revision,
            report: &report,
        };
        let oversized = report.session.projection_oversized
            || !encode(&envelope, &mut bytes, self.shared.limits.report_bytes);
        if oversized {
            #[cfg(test)]
            self.pause_publication(PublicationStage::BeforeOversize);
            {
                let _admission = self.shared.admission.lock();
                if encoding_epoch != 0
                    && self.shared.recording_epoch.load(Ordering::Acquire) == encoding_epoch
                {
                    effects.oversized_report(&self.shared);
                    oversize_accounted = true;
                }
            }
            #[cfg(test)]
            self.pause_publication(PublicationStage::AfterOversize);
            let emergency = report.emergency();
            let envelope = ReportRecord {
                schema: 1,
                record_type: "report",
                instance_id: &self.shared.instance_id,
                report_revision: report.revision,
                report: &emergency,
            };
            if !encode(&envelope, &mut bytes, self.shared.limits.report_bytes) {
                // Retain the new identity/revision as unavailable instead of
                // silently associating future events with an older session.
                bytes.clear();
            }
        }
        let prepared = Arc::new(PreparedReport {
            revision: report.revision,
            identity: report.session.identity,
            bytes,
        });
        #[cfg(test)]
        self.pause_publication(PublicationStage::BeforeAdmission);
        let _admission = self.shared.admission.lock();
        if !self.shared.accepting.load(Ordering::Acquire) {
            effects.error(
                &self.shared,
                DiagnosticsError::new(
                    DiagnosticsOperation::Enqueue,
                    DiagnosticsCode::WorkerStopped,
                ),
            );
            return effects;
        }
        if self.shared.revision_exhausted.load(Ordering::Acquire) {
            effects.error(
                &self.shared,
                DiagnosticsError::new(
                    DiagnosticsOperation::Enqueue,
                    DiagnosticsCode::CounterExhausted,
                ),
            );
            return effects;
        }
        let mut current = self.shared.report.lock();
        if prepared.revision <= current.revision {
            effects.error(
                &self.shared,
                DiagnosticsError::new(
                    DiagnosticsOperation::Enqueue,
                    DiagnosticsCode::InvalidReportRevision,
                ),
            );
            return effects;
        }
        *current = Arc::clone(&prepared);
        drop(current);
        // A newly admitted report supersedes an older overflow/bootstrap slot,
        // without changing the reports already referenced by queued events.
        *self.shared.latest_report.lock() = None;
        let epoch = self.shared.recording_epoch.load(Ordering::Acquire);
        if epoch == 0 {
            return effects;
        }
        if oversized && !oversize_accounted {
            effects.oversized_report(&self.shared);
        }
        if prepared.bytes.is_empty() {
            return effects;
        }
        match self.sender.try_send(Message::Report {
            epoch,
            report: Arc::clone(&prepared),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                *self.shared.latest_report.lock() = Some((epoch, prepared));
                effects.error(
                    &self.shared,
                    DiagnosticsError::new(
                        DiagnosticsOperation::Enqueue,
                        DiagnosticsCode::QueueFull,
                    ),
                );
            }
            Err(TrySendError::Disconnected(_)) => effects.error(
                &self.shared,
                DiagnosticsError::new(
                    DiagnosticsOperation::Enqueue,
                    DiagnosticsCode::WorkerStopped,
                ),
            ),
        }
        self.shared.wake();
        effects
    }
    #[cfg(test)]
    fn pause_publication(&self, stage: PublicationStage) {
        let pause = {
            let mut slot = self.shared.publication_pause.lock();
            if slot.as_ref().is_some_and(|pause| pause.stage == stage) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(pause) = pause {
            pause
                .reached
                .send(())
                .expect("publication barrier observer retained");
            pause.resume.recv().expect("publication barrier released");
        }
    }
    pub fn event(&self, level: EventLevel, event_code: &'static str, fields: &[SafeField]) {
        let epoch = self.shared.recording_epoch.load(Ordering::Acquire);
        if epoch == 0 {
            return;
        }
        self.submit(
            epoch,
            level,
            known_code(event_code).unwrap_or("fields_omitted"),
            if known_code(event_code).is_some() {
                fields
            } else {
                &[]
            },
        );
    }
    /// Called only under admission; mismatch is cancellation, not event loss.
    fn admit_event(&self, epoch: u64) -> bool {
        if epoch == 0 || self.shared.recording_epoch.load(Ordering::Acquire) != epoch {
            return false;
        }
        if !self.shared.accepting.load(Ordering::Acquire) {
            self.shared.dropped(DiagnosticsCode::WorkerStopped);
            return false;
        }
        if self.shared.revision_exhausted.load(Ordering::Acquire) {
            self.shared.dropped(DiagnosticsCode::CounterExhausted);
            return false;
        }
        true
    }
    fn reject_oversized_record(&self, epoch: u64) -> bool {
        let _admission = self.shared.admission.lock();
        if !self.admit_event(epoch) {
            return false;
        }
        let mut health = self.shared.health.lock();
        health.oversized_records = health.oversized_records.saturating_add(1);
        health.dropped_events = health.dropped_events.saturating_add(1);
        health.last_error = Some(DiagnosticsError::new(
            DiagnosticsOperation::Encode,
            DiagnosticsCode::OversizedRecord,
        ));
        true
    }
    fn submit(
        &self,
        epoch: u64,
        level: EventLevel,
        event_code: &'static str,
        fields: &[SafeField],
    ) {
        let (sequence, report) =
            {
                let _admission = self.shared.admission.lock();
                if !self.admit_event(epoch) {
                    return;
                }
                let Ok(sequence) = self.shared.sequence.fetch_update(
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                    |value| value.checked_add(1),
                ) else {
                    self.shared.dropped(DiagnosticsCode::CounterExhausted);
                    return;
                };
                let report = Arc::clone(&self.shared.report.lock());
                if report.bytes.is_empty() {
                    self.shared.dropped(DiagnosticsCode::OversizedReport);
                    return;
                }
                (sequence, report)
            };
        let mut bytes = self.shared.buffer();
        let record = EventRecord {
            schema: 1,
            instance_id: &self.shared.instance_id,
            event_sequence: sequence + 1,
            report_revision: report.revision,
            level: level.label(),
            event_code,
            session_identity: report.identity,
            fields: FieldSlice(fields),
        };
        if fields.len() > MAX_FIELDS || !encode(&record, &mut bytes, self.shared.limits.event_bytes)
        {
            self.shared.recycle(bytes);
            if self.reject_oversized_record(epoch) && event_code != "record_rejected" {
                self.submit(epoch, EventLevel::Warn, "record_rejected", &[]);
            }
            return;
        }
        let _admission = self.shared.admission.lock();
        if !self.admit_event(epoch) {
            self.shared.recycle(bytes);
            return;
        }
        match self.sender.try_send(Message::Event {
            epoch,
            event: EncodedEvent { report, bytes },
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(Message::Event { event, .. })) => {
                self.shared.recycle(event.bytes);
                self.shared.dropped(DiagnosticsCode::QueueFull);
            }
            Err(TrySendError::Disconnected(Message::Event { event, .. })) => {
                self.shared.recycle(event.bytes);
                self.shared.dropped(DiagnosticsCode::WorkerStopped);
            }
            Err(_) => unreachable!("only event messages were submitted"),
        }
        self.shared.wake();
    }
    pub fn prepare_log_folder(&self, request: FolderRequestId) {
        let _admission = self.shared.admission.lock();
        let epoch = self.shared.recording_epoch.load(Ordering::Acquire);
        if epoch == 0 {
            return;
        }
        let mut folders = self.shared.folders.lock();
        let mut health = self.shared.health.lock();
        if health
            .folder
            .request
            .is_some_and(|previous| request <= previous)
        {
            return;
        }
        if health.folder.state == FolderState::Preparing {
            return;
        }
        health.folder.request = Some(request);
        health.folder.error = None;
        health.folder.state = FolderState::Preparing;
        folders.response = None;
        folders.dispatchable = None;
        folders.awaiting_ack = None;
        folders.observing = None;
        if !self.shared.accepting.load(Ordering::Acquire) {
            let error = DiagnosticsError::new(
                DiagnosticsOperation::OpenFolder,
                DiagnosticsCode::WorkerStopped,
            );
            health.folder.state = FolderState::Failed;
            health.folder.error = Some(error);
            folders.response = Some((
                epoch,
                FolderPreparation {
                    request,
                    result: Err(error),
                },
            ));
            return;
        }
        folders.request = Some((epoch, request));
        drop(health);
        drop(folders);
        self.shared.wake();
    }
    pub fn take_folder_result(&self) -> Option<FolderPreparation> {
        let _admission = self.shared.admission.lock();
        let mut folders = self.shared.folders.lock();
        let (epoch, response) = folders.response.take()?;
        if epoch == 0 || self.shared.recording_epoch.load(Ordering::Acquire) != epoch {
            return None;
        }
        if response.result.is_ok() {
            folders.dispatchable = Some((epoch, response.request));
        }
        Some(response)
    }
    /// One-shot authorization immediately before the native folder launcher.
    pub fn claim_log_folder_dispatch(&self, request: FolderRequestId) -> bool {
        let _admission = self.shared.admission.lock();
        let mut folders = self.shared.folders.lock();
        let Some((epoch, pending)) = folders.dispatchable else {
            return false;
        };
        if pending != request
            || epoch == 0
            || self.shared.recording_epoch.load(Ordering::Acquire) != epoch
        {
            return false;
        }
        folders.dispatchable = None;
        folders.awaiting_ack = Some((epoch, request));
        true
    }
    pub fn acknowledge_log_folder(&self, request: FolderRequestId, accepted: bool) {
        let _admission = self.shared.admission.lock();
        let mut folders = self.shared.folders.lock();
        let epoch = self.shared.recording_epoch.load(Ordering::Acquire);
        if epoch == 0 || folders.awaiting_ack != Some((epoch, request)) {
            return;
        }
        let mut health = self.shared.health.lock();
        if health.folder.request != Some(request) {
            return;
        }
        folders.awaiting_ack = None;
        folders.observing = accepted.then_some((epoch, request));
        health.folder.state = if accepted {
            FolderState::OpenRequested
        } else {
            FolderState::Failed
        };
        health.folder.error = if accepted {
            None
        } else {
            Some(DiagnosticsError::new(
                DiagnosticsOperation::OpenFolder,
                DiagnosticsCode::OpenRejected,
            ))
        };
    }
    /// Consume the current launcher's terminal result, never claiming a window opened.
    pub fn finish_log_folder(&self, request: FolderRequestId, failed: bool) {
        let _admission = self.shared.admission.lock();
        let mut folders = self.shared.folders.lock();
        let epoch = self.shared.recording_epoch.load(Ordering::Acquire);
        if epoch == 0 || folders.observing != Some((epoch, request)) {
            return;
        }
        let mut health = self.shared.health.lock();
        if health.folder.request != Some(request) {
            return;
        }
        folders.observing = None;
        if failed {
            health.folder.state = FolderState::Failed;
            health.folder.error = Some(DiagnosticsError::new(
                DiagnosticsOperation::OpenFolder,
                DiagnosticsCode::LauncherFailed,
            ));
        }
    }
    pub fn counter_exhausted(&self) {
        let _admission = self.shared.admission.lock();
        self.shared
            .revision_exhausted
            .store(true, Ordering::Release);
        self.shared.error(DiagnosticsError::new(
            DiagnosticsOperation::Enqueue,
            DiagnosticsCode::CounterExhausted,
        ));
    }
    pub fn reject_folder_counter_exhausted(&self) {
        let _admission = self.shared.admission.lock();
        if self.shared.recording_epoch.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut folders = self.shared.folders.lock();
        let mut health = self.shared.health.lock();
        folders.request = None;
        folders.response = None;
        folders.dispatchable = None;
        folders.awaiting_ack = None;
        folders.observing = None;
        health.folder.state = FolderState::Failed;
        health.folder.error = Some(DiagnosticsError::new(
            DiagnosticsOperation::OpenFolder,
            DiagnosticsCode::CounterExhausted,
        ));
    }
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let config = DiagnosticsConfig {
            xdg_state_home: None,
            home: None,
            limits: LogLimits::default(),
            logging_enabled: true,
        };
        let (handle, receiver) = build(config);
        drop(receiver);
        handle.shared.accepting.store(false, Ordering::Release);
        handle.shared.finished.store(true, Ordering::Release);
        handle
    }
    #[cfg(test)]
    pub(crate) fn test_dispatch(&self) -> tracing::Dispatch {
        tracing::Dispatch::new(tracing_subscriber::registry().with(SafeLayer::new(self.clone())))
    }
    #[cfg(test)]
    pub(super) fn exhaust_report_revisions_for_test(&self) {
        self.shared
            .last_report_revision
            .store(u64::MAX - 1, Ordering::Release);
    }
}

pub struct DiagnosticsRuntime {
    handle: DiagnosticsHandle,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl DiagnosticsRuntime {
    pub fn start(config: DiagnosticsConfig) -> Self {
        let (handle, receiver) = build(config);
        let shared = Arc::clone(&handle.shared);
        let worker = thread::Builder::new()
            .name("furami-diagnostics".into())
            .spawn(move || {
                let worker_thread = thread::current();
                let _ = shared.worker_thread.set(worker_thread);
                Worker::new(shared, receiver, Box::new(io::stderr())).run();
            });
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(error) => {
                handle.shared.storage_error(DiagnosticsError::io(
                    DiagnosticsOperation::Configure,
                    DiagnosticsCode::WorkerStopped,
                    &error,
                ));
                handle.shared.accepting.store(false, Ordering::Release);
                handle.shared.finished.store(true, Ordering::Release);
                None
            }
        };
        Self {
            handle,
            worker: Mutex::new(worker),
        }
    }
    pub fn handle(&self) -> DiagnosticsHandle {
        self.handle.clone()
    }
    pub fn install_tracing(&self) {
        if CALLBACK_HANDLE.set(self.handle()).is_err() {
            self.handle.shared.error(DiagnosticsError::new(
                DiagnosticsOperation::Subscribe,
                DiagnosticsCode::SubscriberUnavailable,
            ));
            return;
        }
        let filter = log_filter(std::env::var_os("RUST_LOG"), &self.handle);
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(SafeLayer::new(self.handle()));
        if tracing::subscriber::set_global_default(subscriber).is_err() {
            self.handle.shared.error(DiagnosticsError::new(
                DiagnosticsOperation::Subscribe,
                DiagnosticsCode::SubscriberUnavailable,
            ));
        }
    }
    pub fn begin_shutdown(&self) {
        let _admission = self.handle.shared.admission.lock();
        self.handle.shared.accepting.store(false, Ordering::Release);
        self.handle.shared.wake();
    }
    pub fn is_finished(&self) -> bool {
        self.worker
            .lock()
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
    }
    /// Call only after Qt returns and media-owner shutdown has been acknowledged.
    /// Never joins a live worker, including on Drop.
    pub fn finish_shutdown(&self, allowance: Duration) {
        self.begin_shutdown();
        let deadline = Instant::now() + allowance.min(SHUTDOWN_TIMEOUT);
        while !self.is_finished() && Instant::now() < deadline {
            thread::park_timeout(
                Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        let mut worker = self.worker.lock();
        if worker.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(worker) = worker.take()
                && worker.join().is_err()
            {
                self.handle.shared.error(DiagnosticsError::new(
                    DiagnosticsOperation::Shutdown,
                    DiagnosticsCode::WorkerStopped,
                ));
            }
        } else if worker.is_some() {
            self.handle.shared.error(DiagnosticsError::new(
                DiagnosticsOperation::Shutdown,
                DiagnosticsCode::ShutdownIncomplete,
            ));
        }
    }
}
impl Drop for DiagnosticsRuntime {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}
static CALLBACK_HANDLE: OnceLock<DiagnosticsHandle> = OnceLock::new();

fn build(config: DiagnosticsConfig) -> (DiagnosticsHandle, Receiver<Message>) {
    let mut health = DiagnosticsHealth::starting(config.limits);
    health.logging_enabled = config.logging_enabled;
    match resolve_location(&config) {
        Ok(location) => health.location = Some(location),
        Err(error) => {
            health.last_error = Some(error);
            health.sink_error = Some(error);
            health.sink_status = SinkStatus::Unavailable;
        }
    }
    let limits = if config.limits.valid() {
        config.limits
    } else {
        health.last_error = Some(DiagnosticsError::new(
            DiagnosticsOperation::Configure,
            DiagnosticsCode::InvalidLimits,
        ));
        health.sink_status = SinkStatus::Unavailable;
        LogLimits::default()
    };
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_nanos())
        .unwrap_or(0);
    static INSTANCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let instance =
        match INSTANCE_SEQUENCE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        }) {
            Ok(instance) => instance,
            Err(_) => {
                let error = DiagnosticsError::new(
                    DiagnosticsOperation::Configure,
                    DiagnosticsCode::CounterExhausted,
                );
                health.last_error = Some(error);
                health.sink_error = Some(error);
                health.sink_status = SinkStatus::Unavailable;
                u64::MAX
            }
        };
    let instance_id = format!("{}-{started:x}-{instance:x}", std::process::id());
    let mut initial = ReportSnapshot {
        revision: ReportRevision(0),
        ..Default::default()
    };
    // Revision zero belongs exclusively to the bootstrap context, independent
    // of the report DTO's default/initial constructor behavior.
    initial.environment.log_location = health.clone();
    let mut bytes = Vec::with_capacity(limits.report_bytes.min(16_384));
    let envelope = ReportRecord {
        schema: 1,
        record_type: "report",
        instance_id: &instance_id,
        report_revision: initial.revision,
        report: &initial,
    };
    if !encode(&envelope, &mut bytes, limits.report_bytes) {
        let fallback = initial.emergency();
        let envelope = ReportRecord {
            schema: 1,
            record_type: "report",
            instance_id: &instance_id,
            report_revision: initial.revision,
            report: &fallback,
        };
        if config.logging_enabled {
            health.oversized_reports = health.oversized_reports.saturating_add(1);
            health.last_error = Some(DiagnosticsError::new(
                DiagnosticsOperation::Encode,
                DiagnosticsCode::OversizedReport,
            ));
        }
        if !encode(&envelope, &mut bytes, limits.report_bytes) {
            bytes.clear();
        }
    }
    let report = Arc::new(PreparedReport {
        revision: initial.revision,
        identity: initial.session.identity,
        bytes,
    });
    let epoch = u64::from(config.logging_enabled);
    let latest_report = if report.bytes.is_empty() || epoch == 0 {
        None
    } else {
        Some((epoch, Arc::clone(&report)))
    };
    let shared = Arc::new(Shared {
        health: Mutex::new(health),
        report: Mutex::new(report),
        latest_report: Mutex::new(latest_report),
        folders: Mutex::new(FolderSlots {
            request: None,
            response: None,
            dispatchable: None,
            awaiting_ack: None,
            observing: None,
        }),
        admission: Mutex::new(Admission {
            last_logging_epoch: epoch,
        }),
        accepting: AtomicBool::new(true),
        recording_epoch: AtomicU64::new(epoch),
        finished: AtomicBool::new(false),
        sequence: AtomicU64::new(0),
        last_report_revision: AtomicU64::new(0),
        revision_exhausted: AtomicBool::new(false),
        worker_thread: OnceLock::new(),
        os: Mutex::new(Observation::default()),
        pool: Mutex::new(Vec::new()),
        instance_id,
        limits,
        #[cfg(test)]
        publication_pause: Mutex::new(None),
    });
    let (sender, receiver) = mpsc::sync_channel(limits.queue_messages);
    (DiagnosticsHandle { shared, sender }, receiver)
}

pub(super) fn resolve_location(
    config: &DiagnosticsConfig,
) -> Result<LogLocation, DiagnosticsError> {
    let (root, origin, suffix) = match config
        .xdg_state_home
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        Some(root) if Path::new(root).is_absolute() => {
            (PathBuf::from(root), LogOrigin::XdgStateHome, "furami/logs")
        }
        Some(_) => {
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Resolve,
                DiagnosticsCode::InvalidStateRoot,
            ));
        }
        None => match config
            .home
            .as_ref()
            .filter(|value| !value.is_empty() && Path::new(value).is_absolute())
        {
            Some(root) => (
                PathBuf::from(root),
                LogOrigin::HomeFallback,
                ".local/state/furami/logs",
            ),
            None => {
                return Err(DiagnosticsError::new(
                    DiagnosticsOperation::Resolve,
                    DiagnosticsCode::MissingHome,
                ));
            }
        },
    };
    let mut normalized = PathBuf::from("/");
    for component in root.join(suffix).components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::CurDir => {}
            Component::Prefix(_) => {
                return Err(DiagnosticsError::new(
                    DiagnosticsOperation::Resolve,
                    DiagnosticsCode::InvalidStateRoot,
                ));
            }
        }
    }
    let path = normalized
        .to_str()
        .filter(|path| !path.as_bytes().contains(&0))
        .ok_or_else(|| {
            DiagnosticsError::new(DiagnosticsOperation::Resolve, DiagnosticsCode::PathEncoding)
        })?;
    Ok(LogLocation {
        path: path.into(),
        origin,
    })
}

pub(super) fn log_filter(value: Option<OsString>, handle: &DiagnosticsHandle) -> EnvFilter {
    let filter = match value {
        None => Ok(EnvFilter::new("info")),
        Some(value) => value
            .to_str()
            .ok_or(())
            .and_then(|value| EnvFilter::try_new(value).map_err(|_| ())),
    };
    filter.unwrap_or_else(|()| {
        handle.shared.error(DiagnosticsError::new(
            DiagnosticsOperation::Configure,
            DiagnosticsCode::InvalidLogFilter,
        ));
        EnvFilter::new("info")
    })
}
pub(super) struct SafeLayer {
    handle: DiagnosticsHandle,
}
impl SafeLayer {
    pub(super) fn new(handle: DiagnosticsHandle) -> Self {
        Self { handle }
    }
}
struct Visitor {
    code: Option<&'static str>,
    fields: [SafeField; MAX_FIELDS],
    length: usize,
    overflow: bool,
}
impl Visitor {
    fn new() -> Self {
        Self {
            code: None,
            fields: [SafeField {
                name: "",
                value: Scalar::Omitted,
            }; MAX_FIELDS],
            length: 0,
            overflow: false,
        }
    }
    fn add(&mut self, field: &Field, value: Scalar) {
        let name = field.name();
        if !numeric_name(name) && !token_name(name) {
            return;
        }
        if self.length == MAX_FIELDS {
            self.overflow = true;
            return;
        }
        self.fields[self.length] = SafeField { name, value };
        self.length += 1;
    }
}
impl Visit for Visitor {
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.add(field, Scalar::Signed(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.add(field, Scalar::Unsigned(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.add(field, Scalar::Bool(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        if value.is_finite() {
            self.add(field, Scalar::Float(value));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "event_code" {
            self.code = known_code(value);
            return;
        }
        if token_name(field.name())
            && let Some(value) = Token::new(value)
        {
            self.add(field, Scalar::Token(value));
        }
    }
}
impl<S: Subscriber> Layer<S> for SafeLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let epoch = self.handle.shared.recording_epoch.load(Ordering::Acquire);
        if epoch == 0 {
            return;
        }
        let mut visitor = Visitor::new();
        event.record(&mut visitor);
        let level = match *event.metadata().level() {
            tracing::Level::TRACE => EventLevel::Trace,
            tracing::Level::DEBUG => EventLevel::Debug,
            tracing::Level::INFO => EventLevel::Info,
            tracing::Level::WARN => EventLevel::Warn,
            tracing::Level::ERROR => EventLevel::Error,
        };
        if visitor.overflow {
            if self.handle.reject_oversized_record(epoch) {
                self.handle.submit(epoch, level, "record_rejected", &[]);
            }
            return;
        }
        if let Some(code) = visitor.code {
            self.handle
                .submit(epoch, level, code, &visitor.fields[..visitor.length]);
        } else {
            self.handle.submit(
                epoch,
                level,
                "fields_omitted",
                &[SafeField::token("target", event.metadata().target())],
            );
        }
    }
}

pub fn diagnostics_qt_message(severity: i32) {
    if let Some(handle) = CALLBACK_HANDLE.get() {
        qt_message(handle, severity);
    }
}
fn qt_message(handle: &DiagnosticsHandle, severity: i32) {
    let epoch = handle.shared.recording_epoch.load(Ordering::Acquire);
    if epoch == 0 {
        return;
    }
    let (level, token) = match severity {
        0 => (EventLevel::Debug, "Debug"),
        1 => (EventLevel::Warn, "Warning"),
        2 => (EventLevel::Error, "Critical"),
        3 => (EventLevel::Error, "Fatal"),
        4 => (EventLevel::Info, "Info"),
        _ => (EventLevel::Warn, "Unknown"),
    };
    handle.submit(
        epoch,
        level,
        "qt_message",
        &[SafeField::token("severity", token)],
    );
}
fn known_control(value: &str) -> bool {
    const CONTROLS: &[&str] = &[
        "UnknownControl",
        "menuOpenLogs",
        "contextDiagnosticsError",
        "furamiRoot",
        "videoHost",
        "topChromeWindow",
        "volumeChromeWindow",
        "bottomChromeWindow",
        "videoContainer",
        "topChromeRoot",
        "volumeChromeRoot",
        "bottomChromeRoot",
        "ribbonSource",
        "ribbonOutput",
        "ribbonResolution",
        "ribbonRate",
        "ribbonFullscreen",
        "volumeSlider",
        "volumeMute",
        "optionsTrigger",
        "menuOpenCapture",
        "menuPauseResume",
        "menuRestart",
        "menuReconnect",
        "menuSettings",
        "menuCloseCapture",
        "menuBack",
        "menuIntentError",
        "settingsCaptureEntry",
        "settingsAudioEntry",
        "settingsRendererEntry",
        "settingsPreferencesEntry",
        "contextIntentError",
        "contextDiagnostic",
        "contextOutputAction",
        "contextRecoveryStage",
        "recoveryUse",
        "sourceRefresh",
        "captureFormatSelector",
        "captureRefresh",
        "audioCaptureEnabled",
        "audioInputSelector",
        "audioMute",
        "audioOutputSelector",
        "audioOutputMute",
        "sizeWidthField",
        "sizeHeightField",
        "rateExactField",
        "sizeRateCatalogError",
        "rendererNotice",
        "preferencesStatus",
        "preferencesStartup",
        "preferencesPath",
        "preferencesImmediate",
        "preferencesAttention",
        "preferencesStatsHidden",
        "preferencesLoggingEnabled",
        "preferencesReset",
        "preferencesTechnicalDetails",
        "sourceCatalogError",
        "sourceCapability",
        "outputMissing",
        "outputStatus",
        "saveFailureRetry",
        "saveFailureClose",
        "resetCancel",
        "resetConfirm",
        "backdropTitle",
        "backdropDetail",
        "backdropDiagnostic",
        "renderChoice_scale",
        "renderChoice_cscale",
        "renderChoice_dscale",
        "renderChoice_scale_antiring",
        "renderChoice_dither",
        "renderChoice_deband",
        "renderChoice_deband_iterations",
        "renderChoice_deband_grain",
        "renderChoice_interpolation",
        "renderToggle_scale",
        "renderToggle_cscale",
        "renderToggle_dscale",
        "renderToggle_scale_antiring",
        "renderToggle_dither",
        "renderToggle_deband",
        "renderToggle_deband_iterations",
        "renderToggle_deband_grain",
        "renderToggle_interpolation",
        "renderValue_scale",
        "renderValue_cscale",
        "renderValue_dscale",
        "renderValue_scale_antiring",
        "renderValue_dither",
        "renderValue_deband",
        "renderValue_deband_iterations",
        "renderValue_deband_grain",
        "renderValue_interpolation",
        "sourceRow_",
        "outputRow_",
        "sizeRow_",
        "rateRow_",
        "captureFormatSelector_row_",
        "audioInputSelector_row_",
        "audioOutputSelector_row_",
        "renderChoice_scale_row_",
        "renderChoice_cscale_row_",
        "renderChoice_dscale_row_",
        "renderChoice_scale_antiring_row_",
        "renderChoice_dither_row_",
        "renderChoice_deband_row_",
        "renderChoice_deband_iterations_row_",
        "renderChoice_deband_grain_row_",
        "renderChoice_interpolation_row_",
    ];
    CONTROLS.contains(&value)
}
// Every native code has its own scalar contract. An empty schema is intentional:
// a lifecycle fact needs no arbitrary message, category or caller-provided data.
const NATIVE_SCHEMAS: &[(&str, &[&str])] = &[
    ("log_folder_dispatched", &["request_id", "accepted"]),
    (
        "log_folder_finished",
        &["request_id", "failed", "exit_code"],
    ),
    (
        "ui_control",
        &[
            "object_name",
            "ordinal",
            "x",
            "y",
            "width",
            "height",
            "visible",
            "enabled",
        ],
    ),
    (
        "qt_geometry",
        &[
            "root",
            "generation",
            "root_width",
            "root_height",
            "video_x",
            "video_y",
            "video_width",
            "video_height",
            "dpr",
        ],
    ),
    (
        "qt_chrome_geometry",
        &[
            "object_name",
            "root",
            "x",
            "y",
            "global_x",
            "global_y",
            "width",
            "height",
            "visible",
            "native_parent_null",
            "transient_root",
        ],
    ),
    ("native_startup_failure", &[]),
    ("native_chrome_failure", &[]),
    (
        "input_focus_window",
        &["object_name", "generation", "root", "active"],
    ),
    ("native_surface_created", &["generation", "poisoned"]),
    ("native_surface_rejected", &["generation"]),
    (
        "native_surface_about_to_destroy",
        &["generation", "host", "authorized"],
    ),
    ("input_mouse_press", &["generation", "button", "x", "y"]),
    ("qualification_stdin_restore_failed", &["errno"]),
    ("native_chrome_registered", &["root", "count"]),
    (
        "input_focus_handoff",
        &["object_name", "backward", "root", "generation"],
    ),
    ("input_text_focus", &["active", "generation"]),
    ("qt_chrome_popup", &["open", "root"]),
    ("native_root", &["root"]),
    (
        "input_focus_observed",
        &["root", "active", "object_name", "generation"],
    ),
    (
        "native_root_destroyed",
        &["root", "generation", "authorized"],
    ),
    (
        "native_root_destroyed_uncontrolled",
        &["root", "generation"],
    ),
    ("qualification_stdin_enabled", &[]),
    ("qualification_stdin_read_failed", &["errno"]),
    ("qualification_stdin_eof", &[]),
    ("qualification_invalid_utf8", &[]),
    ("qualification_line_oversized", &[]),
    ("intent_open_capture", &[]),
    (
        "intent_restart_capture",
        &["generation", "attempt_id", "accepted"],
    ),
    ("intent_reconnect_capture", &["generation", "attempt_id"]),
    (
        "intent_choose_recovery",
        &["generation", "watch", "epoch", "candidate"],
    ),
    ("intent_close_session", &["generation"]),
    ("intent_toggle_pause", &["generation", "accepted"]),
    ("intent_set_volume", &["volume_percent", "accepted"]),
    ("intent_set_muted", &["muted", "accepted"]),
    ("intent_select_output", &["revision", "accepted"]),
    ("intent_open_close_during_opening", &[]),
    ("intent_force_surface_loss", &["root", "host", "generation"]),
    ("intent_application_close", &["root", "generation"]),
    (
        "fullscreen_preference_readback",
        &["effective", "root", "transition"],
    ),
    (
        "fullscreen_transition_cancelled",
        &[
            "reason",
            "requested",
            "observed",
            "retained_preference",
            "root",
            "transition",
            "focus_restore_pending",
        ],
    ),
    (
        "intent_toggle_fullscreen",
        &["root", "generation", "fullscreen"],
    ),
    (
        "fullscreen_focus_restored",
        &[
            "root",
            "object_name",
            "native_previously_focused",
            "generation",
        ],
    ),
    ("intent_focus_video", &["root", "host", "generation"]),
    (
        "input_key_press",
        &["source", "repeat", "text_entry", "popup", "generation"],
    ),
    (
        "input_forward",
        &["source", "event_type", "text_entry", "generation"],
    ),
    ("qt_quit_veto", &[]),
    ("qt_root_close_veto", &["root", "generation"]),
    (
        "fullscreen_native_state_observed",
        &["requested", "effective", "matched", "root", "transition"],
    ),
    (
        "ui_phase",
        &[
            "gate_phase",
            "generation",
            "restart_generation",
            "failed",
            "paused",
            "volume_percent",
            "muted",
            "can_restart",
            "can_toggle_pause",
            "can_set_gain",
        ],
    ),
    ("ui_stop_ack", &["generation", "root", "host"]),
    ("qt_quit_authorized", &["generation", "root"]),
    (
        "native_object_destroyed",
        &["generation", "host", "authorized"],
    ),
    ("native_create", &["generation", "root"]),
    (
        "native_publication_rejected",
        &["generation", "current", "poisoned"],
    ),
    (
        "native_publish",
        &["root", "host", "generation", "x11_parent"],
    ),
    ("proof_close_during_opening", &["generation", "accepted"]),
    (
        "native_surface_loss",
        &["generation", "host", "root", "controlled"],
    ),
    ("native_loss_barrier_begin", &["generation", "host"]),
    ("native_loss_barrier_failed", &["generation", "host"]),
    ("native_loss_barrier_complete", &["generation", "host"]),
    ("native_failure", &["generation", "root", "host"]),
    ("native_release_rejected", &["generation", "current"]),
    (
        "native_destroy_begin",
        &["generation", "host", "root", "parent_preserved"],
    ),
    (
        "native_destroy",
        &["generation", "host", "root", "authorized", "poisoned"],
    ),
    ("qt_application_destroy", &[]),
    ("native_startup", &["shape_major", "shape_minor"]),
    ("qt_engine_destroy", &[]),
    ("qt_uncontrolled_event_loop_exit", &[]),
    (
        "qt_event_loop_exit",
        &["exit_code", "owner_cleanup_authorized"],
    ),
];
fn native_schema(code: &str) -> Option<(&'static str, &'static [&'static str])> {
    NATIVE_SCHEMAS
        .iter()
        .copied()
        .find(|(candidate, _)| *candidate == code)
}
fn native_bool(name: &str) -> bool {
    matches!(
        name,
        "visible"
            | "enabled"
            | "native_parent_null"
            | "transient_root"
            | "active"
            | "poisoned"
            | "authorized"
            | "backward"
            | "open"
            | "accepted"
            | "muted"
            | "effective"
            | "requested"
            | "observed"
            | "retained_preference"
            | "focus_restore_pending"
            | "fullscreen"
            | "native_previously_focused"
            | "repeat"
            | "text_entry"
            | "popup"
            | "matched"
            | "failed"
            | "paused"
            | "can_restart"
            | "can_toggle_pause"
            | "can_set_gain"
            | "controlled"
            | "parent_preserved"
            | "owner_cleanup_authorized"
    )
}
fn reject_native_record(handle: &DiagnosticsHandle, epoch: u64) {
    if handle.reject_oversized_record(epoch) {
        handle.submit(epoch, EventLevel::Warn, "record_rejected", &[]);
    }
}
pub fn diagnostics_native_event(code: &str, severity: i32, fields_json: &str) {
    let Some(handle) = CALLBACK_HANDLE.get() else {
        return;
    };
    native_event(handle, code, severity, fields_json);
}
pub(super) fn native_event(
    handle: &DiagnosticsHandle,
    code: &str,
    severity: i32,
    fields_json: &str,
) {
    let epoch = handle.shared.recording_epoch.load(Ordering::Acquire);
    if epoch == 0 {
        return;
    }
    let Some((code, schema)) = native_schema(code) else {
        handle.submit(epoch, EventLevel::Warn, "native_payload_rejected", &[]);
        return;
    };
    if fields_json.len() > handle.shared.limits.event_bytes {
        reject_native_record(handle, epoch);
        return;
    }
    let Ok(serde_json::Value::Object(payload)) = serde_json::from_str(fields_json) else {
        handle.submit(epoch, EventLevel::Warn, "native_payload_rejected", &[]);
        return;
    };
    let mut fields = [SafeField {
        name: "",
        value: Scalar::Omitted,
    }; MAX_FIELDS];
    let mut length = 0;
    for (name, value) in payload {
        let Some(name) = schema.iter().copied().find(|candidate| *candidate == name) else {
            continue;
        };
        if length == MAX_FIELDS {
            reject_native_record(handle, epoch);
            return;
        }
        let field = match value {
            serde_json::Value::Bool(value) if native_bool(name) => SafeField::bool(name, value),
            serde_json::Value::Number(value) if !native_bool(name) && !token_name(name) => {
                if let Some(value) = value.as_u64() {
                    SafeField::u64(name, value)
                } else if matches!(
                    name,
                    "errno"
                        | "exit_code"
                        | "volume_percent"
                        | "x"
                        | "y"
                        | "global_x"
                        | "global_y"
                        | "video_x"
                        | "video_y"
                ) && let Some(value) = value.as_i64()
                {
                    SafeField::i64(name, value)
                } else if matches!(
                    name,
                    "x" | "y"
                        | "width"
                        | "height"
                        | "video_x"
                        | "video_y"
                        | "video_width"
                        | "video_height"
                        | "dpr"
                ) && let Some(value) = value.as_f64()
                {
                    SafeField::f64(name, value)
                } else {
                    continue;
                }
            }
            serde_json::Value::String(value) if token_name(name) && token_allowed(name, &value) => {
                SafeField::token(name, &value)
            }
            _ => continue,
        };
        fields[length] = field;
        length += 1;
    }
    let level = match severity {
        0 => EventLevel::Debug,
        1 => EventLevel::Warn,
        2 | 3 => EventLevel::Error,
        _ => EventLevel::Info,
    };
    handle.submit(epoch, level, code, &fields[..length]);
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentHeader<'a> {
    schema: u8,
    record_type: &'a str,
    application: &'a str,
    segment: u64,
}
#[derive(Clone, Copy)]
struct Segment {
    device: u64,
    inode: u64,
    number: u64,
    length: u64,
    repaired: bool,
}
#[derive(Clone, Copy)]
struct Checkpoint {
    device: u64,
    inode: u64,
    segment: u64,
    revision: ReportRevision,
    length: u64,
    offset: u64,
}
#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) enum Failpoint {
    PartialWrite(usize),
    Rotation,
    LockUnsupported,
    Rollback,
    DirectoryAccess,
}
struct Storage {
    root: PathBuf,
    limits: LogLimits,
    shared: Arc<Shared>,
    checkpoint: Option<Checkpoint>,
    scratch: Vec<u8>,
    lock_timeout: Duration,
    #[cfg(test)]
    failpoint: Option<Failpoint>,
}
impl Storage {
    fn new(root: PathBuf, limits: LogLimits, shared: Arc<Shared>) -> Self {
        Self {
            root,
            limits,
            shared,
            checkpoint: None,
            scratch: Vec::new(),
            lock_timeout: LOCK_TIMEOUT,
            #[cfg(test)]
            failpoint: None,
        }
    }
    fn append(
        &mut self,
        report: &PreparedReport,
        event: Option<&[u8]>,
    ) -> Result<(), DiagnosticsError> {
        #[cfg(test)]
        if matches!(self.failpoint, Some(Failpoint::DirectoryAccess)) {
            self.failpoint = None;
            return Err(DiagnosticsError::io(
                DiagnosticsOperation::Directory,
                DiagnosticsCode::DirectoryAccess,
                &io::Error::from(io::ErrorKind::PermissionDenied),
            ));
        }
        let directory = open_directory(&self.root)?;
        let lock = open_entry(&directory, LOCK_NAME, true)?.ok_or_else(|| {
            DiagnosticsError::new(DiagnosticsOperation::Lock, DiagnosticsCode::UnsafeEntry)
        })?;
        validate_file(&lock, DiagnosticsOperation::Lock)?;
        let lock_metadata = lock.metadata().map_err(|error| {
            DiagnosticsError::io(
                DiagnosticsOperation::Lock,
                DiagnosticsCode::LockUnsupported,
                &error,
            )
        })?;
        if lock_metadata.len() != 0 {
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Lock,
                DiagnosticsCode::UnsafeEntry,
            ));
        }
        if lock_metadata.mode() & 0o777 != 0o600 {
            lock.set_permissions(Permissions::from_mode(0o600))
                .map_err(|error| {
                    DiagnosticsError::io(
                        DiagnosticsOperation::Lock,
                        DiagnosticsCode::LockUnsupported,
                        &error,
                    )
                })?;
        }
        #[cfg(test)]
        if matches!(self.failpoint, Some(Failpoint::LockUnsupported)) {
            self.failpoint = None;
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Lock,
                DiagnosticsCode::LockUnsupported,
            ));
        }
        let started = Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() >= self.lock_timeout {
                        return Err(DiagnosticsError::new(
                            DiagnosticsOperation::Lock,
                            DiagnosticsCode::LockContended,
                        ));
                    }
                    if !self.shared.accepting.load(Ordering::Acquire) {
                        return Err(DiagnosticsError::new(
                            DiagnosticsOperation::Shutdown,
                            DiagnosticsCode::ShutdownIncomplete,
                        ));
                    }
                    thread::park_timeout(Duration::from_millis(10));
                }
                Err(TryLockError::Error(error)) => {
                    return Err(DiagnosticsError::io(
                        DiagnosticsOperation::Lock,
                        DiagnosticsCode::LockUnsupported,
                        &error,
                    ));
                }
            }
        }
        // An unlinked/replaced lock must never permit writes in a second lock domain.
        let current_lock = open_entry(&directory, LOCK_NAME, false)?.ok_or_else(|| {
            DiagnosticsError::new(DiagnosticsOperation::Lock, DiagnosticsCode::UnsafeEntry)
        })?;
        let current_metadata = current_lock.metadata().map_err(|error| {
            DiagnosticsError::io(
                DiagnosticsOperation::Lock,
                DiagnosticsCode::UnsafeEntry,
                &error,
            )
        })?;
        if (lock_metadata.dev(), lock_metadata.ino())
            != (current_metadata.dev(), current_metadata.ino())
        {
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Lock,
                DiagnosticsCode::UnsafeEntry,
            ));
        }
        drop(current_lock);
        let mut segments: [Option<Segment>; MAX_FILES] = [None; MAX_FILES];
        for (index, segment) in segments.iter_mut().enumerate() {
            *segment = self.inspect(&directory, managed_name(index), true)?;
        }
        let maximum = segments
            .iter()
            .flatten()
            .map(|segment| segment.number)
            .max()
            .unwrap_or(0);
        for (index, segment) in segments.iter_mut().enumerate().skip(self.limits.files) {
            if segment.is_some() {
                remove_entry(
                    &directory,
                    managed_name(index),
                    DiagnosticsOperation::Rotate,
                )?;
                *segment = None;
            }
        }
        let mut active = match segments[0] {
            Some(segment) => segment,
            None => self.create_segment(
                &directory,
                maximum.checked_add(1).ok_or_else(|| {
                    DiagnosticsError::new(
                        DiagnosticsOperation::Rotate,
                        DiagnosticsCode::CounterExhausted,
                    )
                })?,
            )?,
        };
        segments[0] = Some(active);
        let mut checkpoint_valid = self.checkpoint.is_some_and(|checkpoint| {
            checkpoint.device == active.device
                && checkpoint.inode == active.inode
                && checkpoint.segment == active.number
                && checkpoint.revision == report.revision
                && active.length >= checkpoint.length
                && !active.repaired
        });
        if checkpoint_valid {
            let checkpoint = self.checkpoint.expect("validated checkpoint");
            let mut file = open_entry(&directory, managed_name(0), false)?.ok_or_else(|| {
                DiagnosticsError::new(DiagnosticsOperation::Validate, DiagnosticsCode::UnsafeEntry)
            })?;
            self.scratch.resize(report.bytes.len(), 0);
            file.seek(SeekFrom::Start(checkpoint.offset))
                .and_then(|_| file.read_exact(&mut self.scratch))
                .map_err(|error| {
                    DiagnosticsError::io(
                        DiagnosticsOperation::Validate,
                        DiagnosticsCode::UnsafeEntry,
                        &error,
                    )
                })?;
            checkpoint_valid = self.scratch == report.bytes;
        }
        if !checkpoint_valid {
            self.checkpoint = None;
        }
        let event_length = event.map_or(0, <[u8]>::len);
        let mut needs_report = !checkpoint_valid;
        let required = event_length as u64
            + if needs_report {
                report.bytes.len() as u64
            } else {
                0
            };
        if active
            .length
            .checked_add(required)
            .is_none_or(|length| length > self.limits.file_bytes)
        {
            let next = maximum.max(active.number).checked_add(1).ok_or_else(|| {
                DiagnosticsError::new(
                    DiagnosticsOperation::Rotate,
                    DiagnosticsCode::CounterExhausted,
                )
            })?;
            // Reserve the full checkpoint+event in the NEW segment before any rename.
            let header_length = segment_bytes(next)?.len() as u64;
            if header_length + report.bytes.len() as u64 + event_length as u64
                > self.limits.file_bytes
            {
                return Err(DiagnosticsError::new(
                    DiagnosticsOperation::Encode,
                    if event.is_some() {
                        DiagnosticsCode::OversizedRecord
                    } else {
                        DiagnosticsCode::OversizedReport
                    },
                ));
            }
            self.rotate(&directory, &segments)?;
            active = self.create_segment(&directory, next)?;
            self.checkpoint = None;
            needs_report = true;
        }
        if report.bytes.is_empty() {
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Encode,
                DiagnosticsCode::OversizedReport,
            ));
        }
        let mut file = open_entry(&directory, managed_name(0), false)?.ok_or_else(|| {
            DiagnosticsError::new(DiagnosticsOperation::Write, DiagnosticsCode::UnsafeEntry)
        })?;
        validate_file(&file, DiagnosticsOperation::Write)?;
        let metadata = file.metadata().map_err(|error| {
            DiagnosticsError::io(
                DiagnosticsOperation::Write,
                DiagnosticsCode::WriteFailed,
                &error,
            )
        })?;
        if metadata.dev() != active.device
            || metadata.ino() != active.inode
            || metadata.len() != active.length
        {
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Write,
                DiagnosticsCode::UnsafeEntry,
            ));
        }
        file.seek(SeekFrom::Start(active.length)).map_err(|error| {
            DiagnosticsError::io(
                DiagnosticsOperation::Write,
                DiagnosticsCode::WriteFailed,
                &error,
            )
        })?;
        #[cfg(test)]
        let injected = match self.failpoint {
            Some(Failpoint::PartialWrite(_) | Failpoint::Rollback) => self.failpoint.take(),
            _ => None,
        };
        #[cfg(test)]
        if let Some(Failpoint::PartialWrite(count)) = injected {
            let bytes = if needs_report {
                report.bytes.as_slice()
            } else {
                event.unwrap_or(&[])
            };
            let _ = file.write_all(&bytes[..count.min(bytes.len())]);
            let failure =
                DiagnosticsError::new(DiagnosticsOperation::Write, DiagnosticsCode::WriteFailed);
            if let Err(error) = file.set_len(active.length) {
                self.shared.health.lock().rollback_error = Some(DiagnosticsError::io(
                    DiagnosticsOperation::Rollback,
                    DiagnosticsCode::WriteFailed,
                    &error,
                ));
            }
            self.checkpoint = None;
            return Err(failure);
        }
        #[cfg(test)]
        if matches!(injected, Some(Failpoint::Rollback)) {
            self.shared.health.lock().rollback_error = Some(DiagnosticsError::new(
                DiagnosticsOperation::Rollback,
                DiagnosticsCode::WriteFailed,
            ));
            self.checkpoint = None;
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Write,
                DiagnosticsCode::WriteFailed,
            ));
        }
        let write = (|| {
            if needs_report {
                file.write_all(&report.bytes)?;
            }
            if let Some(event) = event {
                file.write_all(event)?;
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(error) = write {
            let failure = DiagnosticsError::io(
                DiagnosticsOperation::Write,
                DiagnosticsCode::WriteFailed,
                &error,
            );
            if let Err(error) = file.set_len(active.length) {
                self.shared.health.lock().rollback_error = Some(DiagnosticsError::io(
                    DiagnosticsOperation::Rollback,
                    DiagnosticsCode::WriteFailed,
                    &error,
                ));
            }
            self.checkpoint = None;
            return Err(failure);
        }
        let length = active.length
            + event_length as u64
            + if needs_report {
                report.bytes.len() as u64
            } else {
                0
            };
        let offset = if needs_report {
            active.length
        } else {
            self.checkpoint.expect("existing checkpoint").offset
        };
        self.checkpoint = Some(Checkpoint {
            device: active.device,
            inode: active.inode,
            segment: active.number,
            revision: report.revision,
            length,
            offset,
        });
        drop(file); // Must precede release of the permanent lock.
        drop(lock);
        Ok(())
    }
    fn inspect(
        &mut self,
        directory: &File,
        name: &str,
        repair: bool,
    ) -> Result<Option<Segment>, DiagnosticsError> {
        let Some(mut file) = open_entry(directory, name, false)? else {
            return Ok(None);
        };
        let metadata = file.metadata().map_err(|error| {
            DiagnosticsError::io(
                DiagnosticsOperation::Validate,
                DiagnosticsCode::UnsafeEntry,
                &error,
            )
        })?;
        let mut header_bytes = [0; HEADER_BYTES];
        let header_read = metadata.len().min(HEADER_BYTES as u64) as usize;
        file.read_exact(&mut header_bytes[..header_read])
            .map_err(|error| {
                DiagnosticsError::io(
                    DiagnosticsOperation::Validate,
                    DiagnosticsCode::UnsafeEntry,
                    &error,
                )
            })?;
        let header_end = header_bytes[..header_read]
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| {
                DiagnosticsError::new(DiagnosticsOperation::Validate, DiagnosticsCode::UnsafeEntry)
            })?;
        let header: SegmentHeader<'_> = serde_json::from_slice(&header_bytes[..header_end])
            .map_err(|_| {
                DiagnosticsError::new(DiagnosticsOperation::Validate, DiagnosticsCode::UnsafeEntry)
            })?;
        if header.schema != 1
            || header.record_type != "segment"
            || header.application != "furami"
            || header.segment == 0
        {
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Validate,
                DiagnosticsCode::UnsafeEntry,
            ));
        }
        // Recovery inspects at most one maximum-record-sized window at a
        // time, including when an externally oversized recognized file exists.
        let cutoff = metadata.len().min(self.limits.file_bytes);
        let start = self.read_tail(&mut file, cutoff)?;
        let complete = start
            + self
                .scratch
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map(|index| index as u64 + 1)
                .ok_or_else(|| {
                    DiagnosticsError::new(
                        DiagnosticsOperation::Repair,
                        DiagnosticsCode::UnsafeEntry,
                    )
                })?;
        if complete < header_end as u64 + 1 {
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Repair,
                DiagnosticsCode::UnsafeEntry,
            ));
        }
        let incomplete = metadata.len() != complete;
        let record_limit = self
            .limits
            .event_bytes
            .max(self.limits.report_bytes)
            .max(HEADER_BYTES) as u64;
        if incomplete
            && metadata.len() <= self.limits.file_bytes
            && metadata.len() - complete > record_limit
        {
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Repair,
                DiagnosticsCode::UnsafeEntry,
            ));
        }
        self.read_tail(&mut file, complete)?;
        let line_end = self.scratch.len() - 1;
        let line_start = self.scratch[..line_end]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        serde_json::from_slice::<serde::de::IgnoredAny>(&self.scratch[line_start..line_end])
            .map_err(|_| {
                DiagnosticsError::new(DiagnosticsOperation::Validate, DiagnosticsCode::UnsafeEntry)
            })?;
        if incomplete && repair {
            file.set_len(complete).map_err(|error| {
                DiagnosticsError::io(
                    DiagnosticsOperation::Repair,
                    DiagnosticsCode::WriteFailed,
                    &error,
                )
            })?;
        }
        if repair && metadata.mode() & 0o777 != 0o600 {
            file.set_permissions(Permissions::from_mode(0o600))
                .map_err(|error| {
                    DiagnosticsError::io(
                        DiagnosticsOperation::Validate,
                        DiagnosticsCode::DirectoryAccess,
                        &error,
                    )
                })?;
        }
        Ok(Some(Segment {
            device: metadata.dev(),
            inode: metadata.ino(),
            number: header.segment,
            length: if incomplete && repair {
                complete
            } else {
                metadata.len()
            },
            repaired: incomplete && repair,
        }))
    }
    fn read_tail(&mut self, file: &mut File, end: u64) -> Result<u64, DiagnosticsError> {
        let maximum = self
            .limits
            .event_bytes
            .max(self.limits.report_bytes)
            .max(HEADER_BYTES) as u64;
        let start = end.saturating_sub(maximum);
        self.scratch.resize((end - start) as usize, 0);
        file.seek(SeekFrom::Start(start))
            .and_then(|_| file.read_exact(&mut self.scratch))
            .map_err(|error| {
                DiagnosticsError::io(
                    DiagnosticsOperation::Validate,
                    DiagnosticsCode::UnsafeEntry,
                    &error,
                )
            })?;
        Ok(start)
    }
    fn create_segment(
        &mut self,
        directory: &File,
        number: u64,
    ) -> Result<Segment, DiagnosticsError> {
        let name = managed_name(0);
        let name_c = name_c(name)?;
        let mut file = open_at(
            directory.as_raw_fd(),
            &name_c,
            O_RDWR | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK,
            0o600,
        )
        .map_err(|error| {
            DiagnosticsError::io(
                DiagnosticsOperation::Write,
                DiagnosticsCode::WriteFailed,
                &error,
            )
        })?;
        validate_file(&file, DiagnosticsOperation::Write)?;
        let bytes = segment_bytes(number)?;
        if let Err(error) = file.write_all(&bytes) {
            let failure = DiagnosticsError::io(
                DiagnosticsOperation::Write,
                DiagnosticsCode::WriteFailed,
                &error,
            );
            if let Err(error) = file.set_len(0) {
                self.shared.health.lock().rollback_error = Some(DiagnosticsError::io(
                    DiagnosticsOperation::Rollback,
                    DiagnosticsCode::WriteFailed,
                    &error,
                ));
            }
            return Err(failure);
        }
        let metadata = file.metadata().map_err(|error| {
            DiagnosticsError::io(
                DiagnosticsOperation::Write,
                DiagnosticsCode::WriteFailed,
                &error,
            )
        })?;
        self.checkpoint = None;
        Ok(Segment {
            device: metadata.dev(),
            inode: metadata.ino(),
            number,
            length: bytes.len() as u64,
            repaired: false,
        })
    }
    fn rotate(
        &mut self,
        directory: &File,
        segments: &[Option<Segment>; MAX_FILES],
    ) -> Result<(), DiagnosticsError> {
        #[cfg(test)]
        if matches!(self.failpoint, Some(Failpoint::Rotation)) {
            self.failpoint = None;
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Rotate,
                DiagnosticsCode::RotationFailed,
            ));
        }
        let oldest = self.limits.files - 1;
        if segments[oldest].is_some() {
            remove_entry(
                directory,
                managed_name(oldest),
                DiagnosticsOperation::Rotate,
            )?;
        }
        for index in (0..oldest).rev() {
            if segments[index].is_none() {
                continue;
            }
            let old = name_c(managed_name(index))?;
            let new = name_c(managed_name(index + 1))?;
            // All five source/destination entries were opened and validated
            // under this same flock before the first destructive operation.
            if unsafe {
                sys_renameat(
                    directory.as_raw_fd(),
                    old.as_ptr(),
                    directory.as_raw_fd(),
                    new.as_ptr(),
                )
            } != 0
            {
                return Err(DiagnosticsError::io(
                    DiagnosticsOperation::Rotate,
                    DiagnosticsCode::RotationFailed,
                    &io::Error::last_os_error(),
                ));
            }
        }
        self.checkpoint = None;
        Ok(())
    }
    fn prepare_folder(&mut self) -> Result<String, DiagnosticsError> {
        let directory = open_directory(&self.root)?;
        // Validation only: this action never repairs/truncates/deletes entries
        // and cannot make an unavailable storage sink healthy again.
        for index in 0..MAX_FILES {
            self.inspect(&directory, managed_name(index), false)?;
        }
        Ok(self
            .root
            .to_str()
            .expect("resolved location is lossless UTF-8")
            .into())
    }
}
fn segment_bytes(number: u64) -> Result<Vec<u8>, DiagnosticsError> {
    let header = SegmentHeader {
        schema: 1,
        record_type: "segment",
        application: "furami",
        segment: number,
    };
    let mut bytes = Vec::with_capacity(128);
    if !encode(&header, &mut bytes, HEADER_BYTES) {
        return Err(DiagnosticsError::new(
            DiagnosticsOperation::Encode,
            DiagnosticsCode::OversizedRecord,
        ));
    }
    Ok(bytes)
}
fn managed_name(index: usize) -> &'static str {
    [
        "furami.log",
        "furami.1.log",
        "furami.2.log",
        "furami.3.log",
        "furami.4.log",
    ][index]
}
fn name_c(name: &str) -> Result<CString, DiagnosticsError> {
    CString::new(name).map_err(|_| {
        DiagnosticsError::new(DiagnosticsOperation::Validate, DiagnosticsCode::UnsafeEntry)
    })
}
fn open_at(directory: RawFd, name: &CString, flags: i32, mode: u32) -> io::Result<File> {
    // SAFETY: name is NUL-terminated; mode is provided even when O_CREAT is
    // absent; on success ownership of the fresh fd is transferred exactly once.
    let fd = unsafe { sys_openat(directory, name.as_ptr(), flags, mode) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn validate_file(file: &File, operation: DiagnosticsOperation) -> Result<(), DiagnosticsError> {
    let metadata = file
        .metadata()
        .map_err(|error| DiagnosticsError::io(operation, DiagnosticsCode::UnsafeEntry, &error))?;
    if !metadata.is_file() || metadata.uid() != unsafe { sys_geteuid() } || metadata.nlink() != 1 {
        return Err(DiagnosticsError::new(
            operation,
            DiagnosticsCode::UnsafeEntry,
        ));
    }
    Ok(())
}
fn open_entry(
    directory: &File,
    name: &str,
    create: bool,
) -> Result<Option<File>, DiagnosticsError> {
    let name = name_c(name)?;
    let flags = O_RDWR | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK | if create { O_CREAT } else { 0 };
    match open_at(directory.as_raw_fd(), &name, flags, 0o600) {
        Ok(file) => {
            validate_file(&file, DiagnosticsOperation::Validate)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && !create => Ok(None),
        Err(error) => Err(DiagnosticsError::io(
            DiagnosticsOperation::Validate,
            if error.raw_os_error() == Some(40) {
                DiagnosticsCode::UnsafeEntry
            } else {
                DiagnosticsCode::DirectoryAccess
            },
            &error,
        )),
    }
}
fn open_directory(path: &Path) -> Result<File, DiagnosticsError> {
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
        .open("/")
        .map_err(|error| {
            DiagnosticsError::io(
                DiagnosticsOperation::Directory,
                DiagnosticsCode::DirectoryAccess,
                &error,
            )
        })?;
    let components = || {
        path.components().filter_map(|component| {
            if let Component::Normal(name) = component {
                Some(name)
            } else {
                None
            }
        })
    };
    let component_count = components().count();
    for (index, component) in components().enumerate() {
        let name = CString::new(component.as_bytes()).map_err(|_| {
            DiagnosticsError::new(
                DiagnosticsOperation::Directory,
                DiagnosticsCode::PathEncoding,
            )
        })?;
        let flags = O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK;
        let next = match open_at(directory.as_raw_fd(), &name, flags, 0) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if unsafe { sys_mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(DiagnosticsError::io(
                            DiagnosticsOperation::Directory,
                            DiagnosticsCode::DirectoryAccess,
                            &error,
                        ));
                    }
                }
                open_at(directory.as_raw_fd(), &name, flags, 0).map_err(|error| {
                    DiagnosticsError::io(
                        DiagnosticsOperation::Directory,
                        DiagnosticsCode::DirectoryAccess,
                        &error,
                    )
                })?
            }
            Err(error) => {
                return Err(DiagnosticsError::io(
                    DiagnosticsOperation::Directory,
                    DiagnosticsCode::DirectoryAccess,
                    &error,
                ));
            }
        };
        let metadata = next.metadata().map_err(|error| {
            DiagnosticsError::io(
                DiagnosticsOperation::Directory,
                DiagnosticsCode::DirectoryAccess,
                &error,
            )
        })?;
        if !metadata.is_dir() {
            return Err(DiagnosticsError::new(
                DiagnosticsOperation::Directory,
                DiagnosticsCode::UnsafeEntry,
            ));
        }
        if index + 2 >= component_count {
            if metadata.uid() != unsafe { sys_geteuid() } {
                return Err(DiagnosticsError::new(
                    DiagnosticsOperation::Directory,
                    DiagnosticsCode::UnsafeEntry,
                ));
            }
            if metadata.mode() & 0o777 != 0o700 {
                next.set_permissions(Permissions::from_mode(0o700))
                    .map_err(|error| {
                        DiagnosticsError::io(
                            DiagnosticsOperation::Directory,
                            DiagnosticsCode::DirectoryAccess,
                            &error,
                        )
                    })?;
            }
        }
        directory = next;
    }
    Ok(directory)
}
fn remove_entry(
    directory: &File,
    name: &str,
    operation: DiagnosticsOperation,
) -> Result<(), DiagnosticsError> {
    let name = name_c(name)?;
    if unsafe { sys_unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(DiagnosticsError::io(
            operation,
            DiagnosticsCode::RotationFailed,
            &io::Error::last_os_error(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkerStage {
    StorageClaimed,
    StorageFinished,
    FolderClaimed,
    FolderFinished,
}
#[cfg(test)]
struct WorkerPause {
    stage: WorkerStage,
    reached: mpsc::Sender<()>,
    resume: Receiver<()>,
}
pub(super) struct Worker {
    shared: Arc<Shared>,
    receiver: Receiver<Message>,
    console: Box<dyn Write + Send>,
    console_available: bool,
    storage: Option<Storage>,
    initialized: bool,
    #[cfg(test)]
    pause: Option<WorkerPause>,
}
impl Worker {
    fn new(
        shared: Arc<Shared>,
        receiver: Receiver<Message>,
        console: Box<dyn Write + Send>,
    ) -> Self {
        let health = shared.health.lock();
        let storage = if health.sink_status != SinkStatus::Unavailable {
            health.location.as_ref().map(|location| {
                Storage::new(
                    PathBuf::from(&location.path),
                    shared.limits,
                    Arc::clone(&shared),
                )
            })
        } else {
            None
        };
        drop(health);
        Self {
            shared,
            receiver,
            console,
            console_available: true,
            storage,
            initialized: false,
            #[cfg(test)]
            pause: None,
        }
    }
    fn initialize(&mut self) {
        if self.initialized {
            return;
        }
        *self.shared.os.lock() = read_os_release();
        self.initialized = true;
    }
    fn run(mut self) {
        loop {
            let full_pass = self.drain_available();
            if !self.shared.accepting.load(Ordering::Acquire) {
                // Admission closed before this flag changed; finish the
                // finite remaining queue without waiting for producers.
                while self.drain_available() {}
                break;
            }
            if !full_pass {
                thread::park_timeout(Duration::from_millis(50));
            }
        }
        self.shared.health.lock().sink_status = SinkStatus::Stopped;
        self.shared.finished.store(true, Ordering::Release);
    }
    pub(super) fn drain_available(&mut self) -> bool {
        self.initialize();
        let mut received = 0;
        // Service the report/folder slots after a bounded batch, even under a
        // producer that continually refills the event queue.
        for _ in 0..self.shared.limits.queue_messages {
            match self.receiver.try_recv() {
                Ok(message) => {
                    self.process(message);
                    received += 1;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let latest = self.shared.latest_report.lock().take();
        if let Some((epoch, report)) = latest {
            self.process(Message::Report { epoch, report });
        }
        let folder_request = self.shared.folders.lock().request.take();
        if let Some((epoch, request)) = folder_request
            && self.shared.claim_effect(epoch)
        {
            #[cfg(test)]
            self.pause_worker(WorkerStage::FolderClaimed);
            let location = self.shared.health.lock().location.clone();
            let result = match location {
                Some(location) => {
                    let mut storage = Storage::new(
                        PathBuf::from(&location.path),
                        self.shared.limits,
                        Arc::clone(&self.shared),
                    );
                    storage.prepare_folder()
                }
                None => {
                    let health = self.shared.health.lock();
                    Err(health.sink_error.unwrap_or_else(|| {
                        DiagnosticsError::new(
                            DiagnosticsOperation::Resolve,
                            DiagnosticsCode::MissingHome,
                        )
                    }))
                }
            };
            #[cfg(test)]
            self.pause_worker(WorkerStage::FolderFinished);
            let _admission = self.shared.admission.lock();
            let mut folders = self.shared.folders.lock();
            let mut health = self.shared.health.lock();
            if self.shared.recording_epoch.load(Ordering::Acquire) == epoch
                && health.folder.request == Some(request)
                && health.folder.state == FolderState::Preparing
            {
                if let Err(error) = result {
                    health.folder.state = FolderState::Failed;
                    health.folder.error = Some(error);
                }
                folders.response = Some((epoch, FolderPreparation { request, result }));
            }
        }
        received == self.shared.limits.queue_messages
    }
    fn process(&mut self, message: Message) {
        let (epoch, report, event) = match &message {
            Message::Report { epoch, report } => (*epoch, report, None),
            Message::Event { epoch, event } => {
                (*epoch, &event.report, Some(event.bytes.as_slice()))
            }
        };
        if self.storage.is_some() && self.shared.claim_effect(epoch) {
            #[cfg(test)]
            self.pause_worker(WorkerStage::StorageClaimed);
            let storage = self.storage.as_mut().expect("storage retained after claim");
            match storage.append(report, event) {
                Ok(()) => {
                    let mut health = self.shared.health.lock();
                    if health.sink_status == SinkStatus::Starting {
                        health.sink_status = SinkStatus::Ready;
                    }
                }
                Err(error) => {
                    self.shared.storage_error(error);
                    self.storage = None;
                }
            }
            #[cfg(test)]
            self.pause_worker(WorkerStage::StorageFinished);
        }
        // No fmt subscriber or raw stderr stream remains alongside this path.
        // Console errors cannot disable a healthy file sink and never recurse.
        if self.console_available && self.shared.claim_effect(epoch) {
            let result = (|| {
                if matches!(&message, Message::Report { .. }) {
                    self.console.write_all(&report.bytes)?;
                }
                if let Some(event) = event {
                    self.console.write_all(event)?;
                }
                Ok::<(), io::Error>(())
            })();
            if let Err(error) = result {
                self.console_available = false;
                self.shared.error(DiagnosticsError::io(
                    DiagnosticsOperation::Console,
                    DiagnosticsCode::ConsoleWriteFailed,
                    &error,
                ));
            }
        }
        if let Message::Event { event, .. } = message {
            self.shared.recycle(event.bytes);
        }
    }
    #[cfg(test)]
    fn pause_worker(&mut self, stage: WorkerStage) {
        if self
            .pause
            .as_ref()
            .is_some_and(|pause| pause.stage == stage)
        {
            let pause = self.pause.take().expect("matching pause retained");
            pause
                .reached
                .send(())
                .expect("worker barrier observer retained");
            pause.resume.recv().expect("worker barrier released");
        }
    }
    #[cfg(test)]
    pub(super) fn pause_at(&mut self, stage: WorkerStage) -> (Receiver<()>, mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        self.pause = Some(WorkerPause {
            stage,
            reached: reached_tx,
            resume: resume_rx,
        });
        (reached_rx, resume_tx)
    }
    #[cfg(test)]
    pub(super) fn set_failpoint(&mut self, failpoint: Failpoint) {
        if let Some(storage) = &mut self.storage {
            storage.failpoint = Some(failpoint);
        }
    }
    #[cfg(test)]
    pub(super) fn set_lock_timeout(&mut self, timeout: Duration) {
        if let Some(storage) = &mut self.storage {
            storage.lock_timeout = timeout;
        }
    }
    #[cfg(test)]
    pub(super) fn process_one_for_test(&mut self) -> bool {
        self.initialize();
        match self.receiver.try_recv() {
            Ok(message) => {
                self.process(message);
                true
            }
            Err(_) => false,
        }
    }
}
#[cfg(test)]
pub(super) fn test_pair(
    config: DiagnosticsConfig,
    console: Box<dyn Write + Send>,
) -> (DiagnosticsHandle, Worker) {
    let (handle, receiver) = build(config);
    let worker = Worker::new(Arc::clone(&handle.shared), receiver, console);
    (handle, worker)
}
#[cfg(test)]
pub(super) fn test_runtime(
    config: DiagnosticsConfig,
    console: Box<dyn Write + Send>,
) -> (DiagnosticsRuntime, Receiver<()>) {
    let (handle, receiver) = build(config);
    let shared = Arc::clone(&handle.shared);
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("furami-diagnostics-test".into())
        .spawn(move || {
            let _ = shared.worker_thread.set(thread::current());
            Worker::new(shared, receiver, console).run();
            let _ = finished_tx.send(());
        })
        .expect("test worker must start");
    (
        DiagnosticsRuntime {
            handle,
            worker: Mutex::new(Some(worker)),
        },
        finished_rx,
    )
}

fn read_os_release() -> Observation<OsReport> {
    let file = match File::open("/etc/os-release") {
        Ok(file) => file,
        Err(error) => {
            return Observation::Unavailable {
                reason: if error.kind() == io::ErrorKind::PermissionDenied {
                    UnavailableReason::AccessDenied
                } else {
                    UnavailableReason::Missing
                },
            };
        }
    };
    let mut bytes = Vec::with_capacity(4_096);
    if file
        .take(OS_RELEASE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Observation::Unavailable {
            reason: UnavailableReason::InvalidData,
        };
    }
    parse_os_release(&bytes)
}
pub(super) fn parse_os_release(bytes: &[u8]) -> Observation<OsReport> {
    if bytes.len() > OS_RELEASE_BYTES {
        return Observation::Unavailable {
            reason: UnavailableReason::Oversized,
        };
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Observation::Unavailable {
            reason: UnavailableReason::InvalidData,
        };
    };
    fn value(text: &str, name: &str) -> Observation<String> {
        let mut result = None;
        for line in text.lines() {
            let Some((key, raw)) = line.split_once('=') else {
                continue;
            };
            if key != name {
                continue;
            }
            if result.is_some() {
                return Observation::Unavailable {
                    reason: UnavailableReason::InvalidData,
                };
            }
            let value = raw
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .or_else(|| {
                    raw.strip_prefix('\'')
                        .and_then(|value| value.strip_suffix('\''))
                })
                .unwrap_or(raw);
            if value.is_empty()
                || value.len() > TOKEN_BYTES
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            {
                return Observation::Unavailable {
                    reason: UnavailableReason::InvalidData,
                };
            }
            result = Some(value.to_owned());
        }
        result
            .map(|value| Observation::Observed { value })
            .unwrap_or(Observation::Unavailable {
                reason: UnavailableReason::Missing,
            })
    }
    Observation::Observed {
        value: OsReport {
            target: "Linux".into(),
            distribution_id: value(text, "ID"),
            distribution_version: value(text, "VERSION_ID"),
        },
    }
}

#[cfg(test)]
#[path = "publication_tests.rs"]
mod publication_tests;
