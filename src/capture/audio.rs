//! Explicit audio source selection and independent per-opening cancellation.

use std::sync::{Arc, atomic::AtomicBool};

use serde::Serialize;

pub use crate::domain::capture::{AudioError, AudioSelection, AudioSourceIdentity, PlaybackGain};

use super::linux::pulse;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioSource {
    pub identity: AudioSourceIdentity,
    pub description: String,
}

/// Owned worker events. A kill acknowledgement is not native teardown proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEvent {
    SourceLost(AudioError),
    CancelFailed(AudioError),
    Cancelled,
}

pub fn discover() -> Result<Vec<AudioSource>, AudioError> {
    pulse::discover()
}

pub fn select(sources: &[AudioSource], name: &str) -> Result<AudioSourceIdentity, AudioError> {
    AudioSourceIdentity::new(name.to_owned(), vec![])?;
    let mut matches = sources
        .iter()
        .filter(|source| source.identity.name() == name);
    let source = matches
        .next()
        .ok_or_else(|| AudioError::SourceMissing { name: name.into() })?;
    if matches.next().is_some() {
        return Err(AudioError::Ambiguous(format!(
            "multiple sources named `{name}`"
        )));
    }
    Ok(source.identity.clone())
}

pub fn revalidate(source: &AudioSourceIdentity) -> Result<(), AudioError> {
    validate_snapshot(source, &discover()?)
}

pub(crate) fn validate_snapshot(
    source: &AudioSourceIdentity,
    observed: &[AudioSource],
) -> Result<(), AudioError> {
    let current = select(observed, source.name())?;
    if !source.compatible_with(&current) {
        return Err(AudioError::SourceChanged {
            name: source.name().into(),
        });
    }
    Ok(())
}

/// Owns an independent libpulse connection and control thread, never an mpv
/// pointer. Start before audio-add; quiesce only after the mpv handle is destroyed.
pub struct RecordingGuard {
    worker: pulse::GuardWorker,
}

impl RecordingGuard {
    pub fn start(
        source: &AudioSourceIdentity,
        generation: u64,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, AudioError> {
        Ok(Self {
            worker: pulse::GuardWorker::start(source.clone(), generation, cancel)?,
        })
    }

    /// Set both FFmpeg Pulse `name` and `stream_name` to this exact value.
    pub fn tag(&self) -> &str {
        self.worker.tag()
    }

    /// Called only after the initial audio-add command's successful reply.
    pub fn mark_open_complete(&self) {
        self.worker.mark_open_complete();
    }

    pub fn poll_event(&self) -> Option<AudioEvent> {
        self.worker.poll_event()
    }

    pub fn quiesce(self) -> Result<(), AudioError> {
        self.worker.quiesce()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_before_guard_start_never_connects_or_creates_recording() {
        let selected = AudioSourceIdentity::new("capture".into(), vec![]).unwrap();
        assert!(matches!(
            RecordingGuard::start(&selected, 7, Arc::new(AtomicBool::new(true))),
            Err(AudioError::Cancelled)
        ));
    }

    fn source(name: &str, serial: &str) -> AudioSource {
        AudioSource {
            identity: AudioSourceIdentity::new(
                name.into(),
                vec![("device.serial".into(), serial.into())],
            )
            .unwrap(),
            description: "Capture".into(),
        }
    }

    #[test]
    fn exact_explicit_source_selection_has_no_default_fallback() {
        let sources = vec![source("capture-a", "a"), source("capture-b", "b")];
        assert_eq!(select(&sources, "capture-b").unwrap().name(), "capture-b");
        assert!(matches!(
            select(&sources, "missing"),
            Err(AudioError::SourceMissing { .. })
        ));
        assert!(select(&sources, "@DEFAULT_SOURCE@").is_err());
    }

    #[test]
    fn duplicate_names_are_ambiguous_even_with_matching_identity() {
        assert!(matches!(
            select(&[source("capture", "a"), source("capture", "a")], "capture"),
            Err(AudioError::Ambiguous(_))
        ));
    }

    #[test]
    fn changed_selected_identity_never_selects_another_source() {
        let selected = source("capture", "a").identity;
        assert!(matches!(
            validate_snapshot(&selected, &[source("capture", "b"), source("other", "a")]),
            Err(AudioError::SourceChanged { .. })
        ));
    }
}
