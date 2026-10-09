//! Responsibility: Logs, versions and capabilities, errors.
//! Allowed dependencies: Cross-cutting instrumentation, never audio/video data.

use crate::domain::{
    failure::{
        FilterAttemptDiagnostics, FilterDiagnosticRecord, FilterEntryMetadata, FilterFailure,
    },
    state::{FilterAttemptKey, FilterConfirmation},
};

/// Structured serialization escapes inert labels and native log controls.
pub(crate) fn filter_begin(key: FilterAttemptKey, entries: &[FilterEntryMetadata]) {
    let event = serde_json::json!({"key": key, "entries": entries});
    tracing::info!(event = %event, "filter_attempt_begin");
}

pub(crate) fn filter_record(key: FilterAttemptKey, record: &FilterDiagnosticRecord) {
    let event = serde_json::json!({"key": key, "record": record});
    tracing::info!(event = %event, "filter_mpv_record");
}

pub(crate) fn filter_end(
    key: FilterAttemptKey,
    result: Result<&FilterConfirmation, &FilterFailure>,
    diagnostics: &FilterAttemptDiagnostics,
) {
    let event = match result {
        Ok(confirmation) => serde_json::json!({
            "key": key, "confirmation": confirmation, "diagnostics": diagnostics,
        }),
        Err(failure) => serde_json::json!({
            "key": key, "failure": failure, "diagnostics": diagnostics,
        }),
    };
    tracing::info!(event = %event, "filter_attempt_end");
}
