//! Cross-cutting, bounded diagnostics. Never owns audio/video data or changes
//! playback admission, media readiness, or owner-thread teardown.
pub mod inventory;
mod logging;
pub mod report;

pub use logging::{
    DiagnosticsCode, DiagnosticsConfig, DiagnosticsError, DiagnosticsHandle, DiagnosticsHealth,
    DiagnosticsOperation, DiagnosticsRuntime, EventLevel, FolderHealth, FolderPreparation,
    FolderRequestId, FolderState, IoKind, LogLimits, LogLocation, LogOrigin, SafeField, SinkStatus,
    diagnostics_native_event, diagnostics_qt_message,
};

#[cfg(test)]
mod logging_tests;
