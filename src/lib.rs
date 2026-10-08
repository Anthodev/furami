// The generated QML initializer links through cxx-qt-lib even when this
// library's Rust modules use no Qt types directly.
extern crate cxx_qt_lib as _;

pub mod app;
pub mod capture;
pub mod domain;
pub mod media;
pub mod native_host;
pub mod settings;
pub mod ui;

// FUR-002 parser evidence lives under tests/support/ as test-only support. It is
// compiled into the library test build (not a standalone Cargo integration-test
// target) so the generated `cxx_qt_init_crate_furami` root is present for the
// globally emitted `--require-defined` linker flag without any artificial
// anchor or unused `extern crate`.
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;
