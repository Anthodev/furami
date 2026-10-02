//! Responsibility: Serialized owner of libmpv, commands and events, errors.
//! Allowed dependencies: `domain`, FFI confined here.

pub mod capabilities;
pub mod controller;
mod ffi;
pub mod session;
