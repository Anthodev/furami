//! FUR-002 parser test support, compiled into the library's `#[cfg(test)]`
//! build via `crate::test_support` and no longer a Cargo integration-test
//! target of its own.

pub mod capabilities;
pub mod media_capability_evidence;
