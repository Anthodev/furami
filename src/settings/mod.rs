//! Responsibility: Versioned local storage, portable import/export.
//! Allowed dependencies: `domain`, serialization and FS.
//!
//! Option A contract: one strict v1 file. A refused original blocks every
//! automatic save until an explicit reset replaces it; session preferences
//! stay session-only until an authorized persistence event writes them.

mod schema;
mod store;

pub use schema::SettingsValidationError;
#[doc(hidden)]
pub use store::WriteFaultPoint;
pub use store::{
    Durability, LoadOutcome, LocalPreferences, SettingsLoadError, SettingsStore,
    SettingsWriteError, StoredDocument, WriteOutcome,
};
