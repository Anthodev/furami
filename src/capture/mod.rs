//! Linux capture discovery, identity, input requests and owned audio cancellation.
//! Allowed dependencies: `domain`, concrete Linux adapter.

pub mod apply;
pub mod audio;
pub mod input;
pub mod linux;

pub use apply::{CaptureValidator, PreparedCapture, validate_prepared};
