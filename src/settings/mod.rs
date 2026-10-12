//! Responsibility: Versioned local storage, portable import/export.
//! Allowed dependencies: `domain`, serialization and FS.
//!
//! Writes use schema v2; strict v1 documents migrate in memory only. A refused
//! original blocks every automatic save until an explicit reset replaces it.
//! Session preferences stay local until an authorized persistence event.

mod profiles;
mod schema;
mod store;

#[cfg(test)]
mod store_tests;

pub use profiles::{ProfileBinding, ProfileValidationError, SourceProfile, SourceProfiles};
pub use schema::SettingsValidationError;
#[doc(hidden)]
pub use store::WriteFaultPoint;
pub use store::{
    Durability, LoadOutcome, LocalPreferences, SettingsLoadError, SettingsStore,
    SettingsWriteError, StoredDocument, WriteOutcome,
};
