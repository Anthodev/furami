//! Responsibility: Serialized owner of libmpv, commands and events, errors.
//! Allowed dependencies: `domain`, FFI confined here.

pub mod controller;
mod ffi;
pub(crate) mod gate;
pub mod session;

/// One host-helper audio owner plus application-lifetime output observation.
#[doc(hidden)]
pub mod loopback;
#[doc(hidden)]
pub mod output_catalog;
