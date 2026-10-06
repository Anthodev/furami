//! Responsibility: SettingsStore lifecycle, refusal, atomic committed writes.
//! Allowed dependencies: `schema`, `domain` value types, FS.
//!
//! Load is all-or-nothing: either the complete document is published or the
//! original file is refused and stays untouched. A refusal blocks every
//! automatic write until an explicit `reset` succeeds — the operator's
//! original bytes are never silently overwritten by new values. Writes go
//! through a temporary file in the destination directory and an atomic
//! rename; once the rename has happened the write is committed and only its
//! durability can be uncertain.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::domain::capture::PlaybackGain;
use crate::domain::output::PersistentOutputChoice;
use crate::domain::state::{AppliedSettings, DraftSettings};

use super::schema::{self, EncodeError, SchemaError};

// ---------------------------------------------------------------------------
// Public documents
// ---------------------------------------------------------------------------

/// Local operator preferences. Session preferences apply immediately but are
/// only written when an authorized persistence event occurs.
///
/// The output choice is the user's persisted preference alone (`Auto` or a
/// manual sink identity); live resolution never becomes part of the persisted
/// value. The field is optional on disk: a strict v1 document without the
/// `output` key decodes to [`PersistentOutputChoice::Auto`]. Downgrade limit:
/// a document written with a manual choice carries that key and is refused by
/// builds that predate it; by design there is no migration path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalPreferences {
    pub gain: PlaybackGain,
    pub fullscreen: bool,
    pub output: PersistentOutputChoice,
}

/// The complete persisted document: last verified applied settings, if any,
/// plus local preferences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDocument {
    pub applied: Option<DraftSettings>,
    pub preferences: LocalPreferences,
}

// ---------------------------------------------------------------------------
// Load outcome and refusals
// ---------------------------------------------------------------------------

/// Result of loading the settings file. Refusal keeps the original file
/// untouched and blocks all automatic writes.
#[derive(Debug)]
pub enum LoadOutcome {
    Missing,
    Loaded(StoredDocument),
    Refused(SettingsLoadError),
}

/// Why a settings file was refused. The original bytes are preserved in every
/// case; only an explicit reset may replace them.
#[derive(Debug, Error)]
pub enum SettingsLoadError {
    #[error("settings file at {path} could not be read: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("settings file at {path} is not valid JSON: {source}")]
    MalformedJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("settings file at {path} violates schema v1: {source}")]
    Schema {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "settings file at {path} uses unsupported schema version {version}; \
         it was never migrated"
    )]
    UnsupportedVersion { path: PathBuf, version: u64 },
    #[error("settings file at {path} contains invalid values: {source}")]
    InvalidValue {
        path: PathBuf,
        #[source]
        source: super::schema::SettingsValidationError,
    },
}

// ---------------------------------------------------------------------------
// Write outcomes and errors
// ---------------------------------------------------------------------------

/// Durability of an already-committed write. The rename succeeded; only the
/// directory sync may be uncertain.
#[derive(Debug)]
pub enum Durability {
    Confirmed,
    Unconfirmed(io::Error),
}

impl Durability {
    pub fn is_confirmed(&self) -> bool {
        matches!(self, Self::Confirmed)
    }

    /// The io error that left durability unconfirmed, if any.
    pub fn error(&self) -> Option<&io::Error> {
        match self {
            Self::Confirmed => None,
            Self::Unconfirmed(error) => Some(error),
        }
    }
}

impl fmt::Display for Durability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Confirmed => formatter.write_str("durably confirmed"),
            Self::Unconfirmed(error) => {
                write!(
                    formatter,
                    "committed but durability was not confirmed: {error}"
                )
            }
        }
    }
}

/// Result of a successful write: always a completed rename.
#[derive(Debug)]
pub enum WriteOutcome {
    Committed { durability: Durability },
}

impl WriteOutcome {
    pub fn durability(&self) -> &Durability {
        match self {
            Self::Committed { durability } => durability,
        }
    }

    pub fn is_confirmed(&self) -> bool {
        self.durability().is_confirmed()
    }
}

impl fmt::Display for WriteOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self::Committed { durability } = self;
        write!(formatter, "settings committed; {durability}")
    }
}

/// Write failures. Every variant is pre-commit: the original file is intact.
#[derive(Debug, Error)]
pub enum SettingsWriteError {
    #[error(
        "settings file at {path} is refused; automatic writes are blocked \
         until an explicit reset"
    )]
    RefusedOriginal { path: PathBuf },
    #[error("settings document failed validation before any write: {source}")]
    InvalidValue {
        #[source]
        source: super::schema::SettingsValidationError,
    },
    #[error("settings document could not be serialized: {source}")]
    Serialize {
        #[source]
        source: serde_json::Error,
    },
    #[error("could not create settings directory at {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not create temporary settings file in {directory}: {source}")]
    CreateTemporary {
        directory: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not write temporary settings file at {path}: {source}")]
    WriteTemporary {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not sync temporary settings file at {path}: {source}")]
    SyncTemporary {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not move temporary settings file at {path} into place: {source}")]
    Rename {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

// ---------------------------------------------------------------------------
// Fault injection (test seam)
// ---------------------------------------------------------------------------

/// Injected write failure points. Hidden from ordinary users; used only by
/// the behavior regressions to prove the original file survives every
/// pre-commit fault.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFaultPoint {
    BeforeWrite,
    PartialWrite,
    BeforeTemporarySync,
    TemporarySyncIo,
    BeforeRename,
    RenameIo,
    ParentOpenIo,
    ParentSyncIo,
}

fn injected_fault(point: WriteFaultPoint) -> io::Error {
    io::Error::other(format!("furami injected test fault: {point:?}"))
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Durable settings storage for one fixed path.
#[derive(Debug)]
pub struct SettingsStore {
    path: PathBuf,
    refused: bool,
    snapshot: Option<StoredDocument>,
    injected_fault: Option<WriteFaultPoint>,
}

impl SettingsStore {
    /// Loads the settings file. The store remembers a refusal (and never
    /// publishes its document) or the last loaded document as snapshot.
    pub fn load(path: PathBuf) -> (Self, LoadOutcome) {
        match fs::read(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => (
                Self {
                    path,
                    refused: false,
                    snapshot: None,
                    injected_fault: None,
                },
                LoadOutcome::Missing,
            ),
            Err(error) => {
                let error = SettingsLoadError::Io {
                    path: path.clone(),
                    source: error,
                };
                (
                    Self {
                        path,
                        refused: true,
                        snapshot: None,
                        injected_fault: None,
                    },
                    LoadOutcome::Refused(error),
                )
            }
            Ok(bytes) => match schema::decode(&bytes) {
                Ok(document) => (
                    Self {
                        path,
                        refused: false,
                        snapshot: Some(document.clone()),
                        injected_fault: None,
                    },
                    LoadOutcome::Loaded(document),
                ),
                Err(schema_error) => {
                    let error = load_error(path.clone(), schema_error);
                    (
                        Self {
                            path,
                            refused: true,
                            snapshot: None,
                            injected_fault: None,
                        },
                        LoadOutcome::Refused(error),
                    )
                }
            },
        }
    }

    /// Loads the ordinary store, then injects a one-shot write fault for tests.
    #[doc(hidden)]
    pub fn with_injected_fault(path: PathBuf, fault: WriteFaultPoint) -> Self {
        let (mut store, _) = Self::load(path);
        store.injected_fault = Some(fault);
        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when the loaded file was refused; every automatic save stays
    /// blocked until an explicit reset succeeds.
    pub fn is_refused(&self) -> bool {
        self.refused
    }

    /// The last document this store loaded or successfully wrote.
    pub fn snapshot(&self) -> Option<&StoredDocument> {
        self.snapshot.as_ref()
    }

    /// Persists verified applied settings plus preferences.
    pub fn save_applied(
        &mut self,
        applied: &AppliedSettings,
        preferences: LocalPreferences,
    ) -> Result<WriteOutcome, SettingsWriteError> {
        if self.refused {
            return Err(SettingsWriteError::RefusedOriginal {
                path: self.path.clone(),
            });
        }
        let document = StoredDocument {
            applied: Some(applied.settings().clone()),
            preferences,
        };
        self.commit(document)
    }

    /// Persists preferences only, preserving the published applied settings.
    pub fn save_preferences(
        &mut self,
        preferences: LocalPreferences,
    ) -> Result<WriteOutcome, SettingsWriteError> {
        if self.refused {
            return Err(SettingsWriteError::RefusedOriginal {
                path: self.path.clone(),
            });
        }
        let applied = self
            .snapshot
            .as_ref()
            .and_then(|stored| stored.applied.clone());
        self.commit(StoredDocument {
            applied,
            preferences,
        })
    }

    /// Explicit, authorized replacement of the whole document: applied is
    /// cleared and preferences are written. The only write allowed to clear
    /// a refusal; refusal is cleared only after the write committed.
    pub fn reset(
        &mut self,
        preferences: LocalPreferences,
    ) -> Result<WriteOutcome, SettingsWriteError> {
        let outcome = self.commit(StoredDocument {
            applied: None,
            preferences,
        })?;
        self.refused = false;
        Ok(outcome)
    }

    // -- internals ----------------------------------------------------------

    fn commit(&mut self, document: StoredDocument) -> Result<WriteOutcome, SettingsWriteError> {
        let bytes = schema::encode(&document).map_err(|error| match error {
            EncodeError::InvalidValue(source) => SettingsWriteError::InvalidValue { source },
            EncodeError::Serialize(source) => SettingsWriteError::Serialize { source },
        })?;
        let outcome = self.atomic_write(&bytes)?;
        self.snapshot = Some(document);
        Ok(outcome)
    }

    /// Temp file in the destination directory, fsync, atomic rename, then a
    /// best-effort directory sync. After the rename the write is committed:
    /// only durability may be uncertain, never the content.
    fn atomic_write(&mut self, bytes: &[u8]) -> Result<WriteOutcome, SettingsWriteError> {
        let fault = self.injected_fault.take();
        let directory = parent_directory(&self.path);
        let file_name = match self.path.file_name().and_then(|name| name.to_str()) {
            Some(name) => name.to_owned(),
            None => {
                return Err(SettingsWriteError::CreateTemporary {
                    directory,
                    source: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "settings path has no usable file name",
                    ),
                });
            }
        };

        fs::create_dir_all(&directory).map_err(|source| SettingsWriteError::CreateDirectory {
            path: directory.clone(),
            source,
        })?;

        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = directory.join(format!(
            "{file_name}.furami-tmp-{}-{unique}",
            std::process::id()
        ));

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|source| SettingsWriteError::CreateTemporary {
                directory: directory.clone(),
                source,
            })?;

        if fault == Some(WriteFaultPoint::BeforeWrite) {
            let error = injected_fault(WriteFaultPoint::BeforeWrite);
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(SettingsWriteError::WriteTemporary {
                path: temporary,
                source: error,
            });
        }

        if fault == Some(WriteFaultPoint::PartialWrite) {
            let partial = &bytes[..bytes.len() / 2];
            let _ = file.write_all(partial);
            let error = injected_fault(WriteFaultPoint::PartialWrite);
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(SettingsWriteError::WriteTemporary {
                path: temporary,
                source: error,
            });
        }

        if let Err(source) = file.write_all(bytes).and_then(|()| file.flush()) {
            let _ = fs::remove_file(&temporary);
            return Err(SettingsWriteError::WriteTemporary {
                path: temporary,
                source,
            });
        }

        if fault == Some(WriteFaultPoint::BeforeTemporarySync) {
            let error = injected_fault(WriteFaultPoint::BeforeTemporarySync);
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(SettingsWriteError::SyncTemporary {
                path: temporary,
                source: error,
            });
        }

        if let Err(source) = file.sync_all() {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(SettingsWriteError::SyncTemporary {
                path: temporary,
                source,
            });
        }

        if fault == Some(WriteFaultPoint::TemporarySyncIo) {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(SettingsWriteError::SyncTemporary {
                path: temporary,
                source: injected_fault(WriteFaultPoint::TemporarySyncIo),
            });
        }

        drop(file);

        if fault == Some(WriteFaultPoint::BeforeRename) {
            let _ = fs::remove_file(&temporary);
            return Err(SettingsWriteError::Rename {
                path: temporary,
                source: injected_fault(WriteFaultPoint::BeforeRename),
            });
        }

        if fault == Some(WriteFaultPoint::RenameIo) {
            let _ = fs::remove_file(&temporary);
            return Err(SettingsWriteError::Rename {
                path: temporary,
                source: injected_fault(WriteFaultPoint::RenameIo),
            });
        }

        fs::rename(&temporary, &self.path).map_err(|source| {
            let _ = fs::remove_file(&temporary);
            SettingsWriteError::Rename {
                path: temporary.clone(),
                source,
            }
        })?;

        // Committed. Parent-directory sync only decides durability.
        let durability = if fault == Some(WriteFaultPoint::ParentOpenIo) {
            Durability::Unconfirmed(injected_fault(WriteFaultPoint::ParentOpenIo))
        } else {
            match File::open(&directory) {
                Ok(directory_file) => {
                    if fault == Some(WriteFaultPoint::ParentSyncIo) {
                        Durability::Unconfirmed(injected_fault(WriteFaultPoint::ParentSyncIo))
                    } else {
                        directory_file
                            .sync_all()
                            .map_or_else(Durability::Unconfirmed, |()| Durability::Confirmed)
                    }
                }
                Err(source) => Durability::Unconfirmed(source),
            }
        };

        Ok(WriteOutcome::Committed { durability })
    }
}

fn parent_directory(path: &Path) -> PathBuf {
    match path.parent() {
        Some(directory) if !directory.as_os_str().is_empty() => directory.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn load_error(path: PathBuf, error: SchemaError) -> SettingsLoadError {
    match error {
        SchemaError::MalformedJson(source) => SettingsLoadError::MalformedJson { path, source },
        SchemaError::Schema(source) => SettingsLoadError::Schema { path, source },
        SchemaError::UnsupportedVersion(version) => {
            SettingsLoadError::UnsupportedVersion { path, version }
        }
        SchemaError::InvalidValue(source) => SettingsLoadError::InvalidValue { path, source },
    }
}
