//! Linux capture discovery, identity, input requests and owned audio cancellation.
//! Allowed dependencies: `domain`, concrete Linux adapter.

pub mod apply;
pub mod audio;
pub mod input;
pub mod linux;
mod watch;

pub(crate) use apply::revalidate_authorized_input;
pub use apply::{CaptureValidator, PreparedCapture, validate_prepared};
