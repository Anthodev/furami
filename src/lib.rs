pub mod app;
pub mod capture;
pub mod diagnostics;
pub mod domain;
pub mod media;
pub mod native_host;
pub mod settings;
pub mod ui;

// FUR-002 parser evidence is shared by library unit tests.
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;
