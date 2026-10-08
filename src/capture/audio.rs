//! Explicit capture source selection and read-only loopback gate safety.

use serde::Serialize;

pub use crate::domain::capture::{AudioError, AudioSelection, AudioSourceIdentity, PlaybackGain};

use super::linux::pulse;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioSource {
    pub identity: AudioSourceIdentity,
    pub description: String,
}

/// Ephemeral, read-only mapping for the loopback owner. Never serialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeSourceSnapshot {
    pub identity: AudioSourceIdentity,
    pub serial: u64,
    pub pulse_index: u32,
    pub observation_revision: u64,
    pub rate: u32,
    /// Pulse channel positions copied from the current source observation.
    pub channel_positions: Vec<i32>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
