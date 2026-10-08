//! Pure output-routing values: validated stable sink identity, live sink
//! targets, catalog observations and desired output plans.
//!
//! Allowed dependencies: `std`, `serde` (Serialize only). No Qt, no Pulse
//! pointers, no live state. Live values (indices, object serials, catalogs)
//! are callback-owned copies and are never persisted; only `SinkIdentity` is
//! persistence-shaped, and persistence must rebuild it through the validating
//! constructor instead of deserializing past validation.

use std::num::NonZeroU64;

use serde::Serialize;

use super::capture::AudioError;

/// Known logical name of the EasyEffects virtual output sink. Auto resolves
/// this output node, not "EasyEffects installed" or a guaranteed active DSP.
pub const EASYEFFECTS_SINK_NAME: &str = "easyeffects_sink";

/// Conservative device property whitelist shared with capture source identity.
/// Pulse indices, object serials, PIDs, client/module ids, sink state, volume,
/// port availability and display descriptions are never identity.
pub const STABLE_SINK_PROPERTY_KEYS: [&str; 7] = [
    "device.serial",
    "device.bus",
    "device.bus_path",
    "device.bus-id",
    "device.vendor.id",
    "device.product.id",
    "device.api",
];

/// libpulse invalid index; never a routable locator.
const PULSE_INVALID_INDEX: u32 = u32::MAX;

/// Exact sink name plus the sorted stable property pairs saved with the
/// user's choice. A name-only identity is the stable logical form of a
/// virtual sink such as EasyEffects; no hardware fingerprint is invented.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SinkIdentity {
    name: String,
    stable_properties: Vec<(String, String)>,
}

impl SinkIdentity {
    /// Validates the name (non-empty, no NUL, not a `@...@` default alias)
    /// and the stable properties (whitelisted keys, non-empty, no NUL, no
    /// duplicate keys), canonicalizing the order by sorting on the key.
    pub fn new(
        name: String,
        mut stable_properties: Vec<(String, String)>,
    ) -> Result<Self, AudioError> {
        if name.trim().is_empty() || name.contains('\0') || name.starts_with('@') {
            return Err(AudioError::InvalidSelection(
                "explicit sink name required".into(),
            ));
        }
        stable_properties.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if stable_properties.iter().any(|(key, value)| {
            key.is_empty() || value.is_empty() || key.contains('\0') || value.contains('\0')
        }) {
            return Err(AudioError::InvalidSelection(
                "invalid stable sink properties".into(),
            ));
        }
        if stable_properties
            .windows(2)
            .any(|pair| pair[0].0 == pair[1].0)
        {
            return Err(AudioError::InvalidSelection(
                "duplicate stable sink property key".into(),
            ));
        }
        if !stable_properties
            .iter()
            .all(|(key, _)| STABLE_SINK_PROPERTY_KEYS.contains(&key.as_str()))
        {
            return Err(AudioError::InvalidSelection(
                "stable sink property outside the allowed whitelist".into(),
            ));
        }
        Ok(Self {
            name,
            stable_properties,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Read-only view of the sorted stable properties, for persistence and
    /// display seams. The vector stays private: identity is built only through
    /// the validating constructor.
    pub fn stable_properties(&self) -> &[(String, String)] {
        &self.stable_properties
    }

    /// Exact saved name plus every saved property pair present with an equal
    /// value in the observed identity. A changed or ambiguous identity is
    /// unavailable, never a same-name substitution.
    pub fn compatible_with(&self, observed: &Self) -> bool {
        self.name == observed.name
            && self.stable_properties.iter().all(|property| {
                observed
                    .stable_properties
                    .binary_search_by(|other| other.0.cmp(&property.0))
                    .is_ok_and(|index| observed.stable_properties[index].1 == property.1)
            })
    }
}

/// Persistent output preference. `Auto` is the default; live resolution is
/// never part of the persisted value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub enum PersistentOutputChoice {
    #[default]
    Auto,
    Manual(SinkIdentity),
}

/// Checked monotonic revision of the desired output plan. It advances when
/// the desired target, choice, silence reason or manual latch changes and
/// lets owners discard stale move completions. Runtime-only, never persisted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct OutputRevision(NonZeroU64);

impl OutputRevision {
    pub fn first() -> Self {
        Self(NonZeroU64::MIN)
    }

    /// Checked increment: exhaustion fails loudly instead of wrapping two
    /// distinct desires onto one revision.
    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("output revision exhausted"))
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Live routing locator for one observed sink: the stable identity plus the
/// current PipeWire object serial and Pulse index. Callback-owned copy,
/// never serialized to preferences.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LiveSinkTarget {
    pub identity: SinkIdentity,
    pub object_serial: NonZeroU64,
    pub pulse_index: u32,
}

impl LiveSinkTarget {
    pub fn new(
        identity: SinkIdentity,
        object_serial: NonZeroU64,
        pulse_index: u32,
    ) -> Result<Self, AudioError> {
        if pulse_index == PULSE_INVALID_INDEX {
            return Err(AudioError::InvalidSelection(
                "live sink target requires a valid Pulse index".into(),
            ));
        }
        Ok(Self {
            identity,
            object_serial,
            pulse_index,
        })
    }
}

/// One catalogued sink. `eligible` means a valid live identity/locator that
/// does not advertise a known-unavailable active port; IDLE/SUSPENDED sinks
/// stay eligible because they can wake and are not absence. Unknown
/// availability remains a routable candidate confirmed by movement/readback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SinkObservation {
    pub target: LiveSinkTarget,
    /// Display only; never an identity match key.
    pub description: String,
    pub eligible: bool,
}

/// Published catalog snapshot. `revision` is the catalog revision used to
/// validate UI rows; it is distinct from [`OutputRevision`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SinkCatalog {
    pub revision: u64,
    pub sinks: Vec<SinkObservation>,
    pub default_sink: Option<SinkIdentity>,
}

/// Typed reason for a silent output plan. Absence and ambiguity never
/// produce an invented destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum OutputSilence {
    /// The catalog itself is unobservable (or not yet observed); detail is a
    /// structured diagnostic string, never a parsed backend message.
    CatalogUnavailable(String),
    /// Auto found no routable candidate: no unique eligible effects sink and
    /// no uniquely resolvable default.
    NoAvailableOutput,
    /// The selected manual identity is absent, changed or ambiguous, with no
    /// fallback.
    ManualUnavailable,
    /// The selected manual identity lost its live target during this run and
    /// returning it is not trusted: a fresh explicit selection is required.
    ManualRequiresAction,
    /// An observed route mismatch or refused move; detail is diagnostic.
    RoutingConflict(String),
}

/// Desired output plan at a checked revision: exactly one of a live target or
/// a typed silence, never both and never neither.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum OutputPlan {
    Target {
        revision: OutputRevision,
        target: LiveSinkTarget,
    },
    Silent {
        revision: OutputRevision,
        reason: OutputSilence,
    },
}

impl OutputPlan {
    pub fn revision(&self) -> OutputRevision {
        match self {
            Self::Target { revision, .. } | Self::Silent { revision, .. } => *revision,
        }
    }

    pub fn target(&self) -> Option<&LiveSinkTarget> {
        match self {
            Self::Target { target, .. } => Some(target),
            Self::Silent { .. } => None,
        }
    }

    pub fn silence(&self) -> Option<&OutputSilence> {
        match self {
            Self::Target { .. } => None,
            Self::Silent { reason, .. } => Some(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, properties: &[(&str, &str)]) -> SinkIdentity {
        SinkIdentity::new(
            name.into(),
            properties
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn rejects_empty_whitespace_and_nul_names() {
        for name in ["", "   ", "sink\0name"] {
            assert!(matches!(
                SinkIdentity::new(name.into(), vec![]),
                Err(AudioError::InvalidSelection(_))
            ));
        }
    }

    #[test]
    fn rejects_default_alias_names() {
        for name in ["@DEFAULT_SINK@", "@DEFAULT_MONITOR@"] {
            assert!(matches!(
                SinkIdentity::new(name.into(), vec![]),
                Err(AudioError::InvalidSelection(_))
            ));
        }
    }

    #[test]
    fn accepts_name_only_virtual_identity() {
        let identity = identity(EASYEFFECTS_SINK_NAME, &[]);
        assert_eq!(identity.name(), EASYEFFECTS_SINK_NAME);
        assert!(identity.stable_properties().is_empty());
    }

    #[test]
    fn rejects_transient_properties_outside_the_whitelist() {
        for key in [
            "object.serial",
            "client.index",
            "core.pid",
            "device.serial2",
        ] {
            assert!(matches!(
                SinkIdentity::new("sink".into(), vec![(key.into(), "1".into())]),
                Err(AudioError::InvalidSelection(_))
            ));
        }
    }

    #[test]
    fn rejects_empty_nul_and_duplicate_property_pairs() {
        assert!(matches!(
            SinkIdentity::new("sink".into(), vec![("device.serial".into(), "".into())]),
            Err(AudioError::InvalidSelection(_))
        ));
        assert!(matches!(
            SinkIdentity::new("sink".into(), vec![("".into(), "1".into())]),
            Err(AudioError::InvalidSelection(_))
        ));
        assert!(matches!(
            SinkIdentity::new("sink".into(), vec![("device.bus".into(), "a\0b".into())]),
            Err(AudioError::InvalidSelection(_))
        ));
        assert!(matches!(
            SinkIdentity::new(
                "sink".into(),
                vec![
                    ("device.serial".into(), "a".into()),
                    ("device.serial".into(), "b".into())
                ]
            ),
            Err(AudioError::InvalidSelection(_))
        ));
    }

    #[test]
    fn canonical_sorting_makes_equality_order_independent() {
        let unsorted = SinkIdentity::new(
            "sink".into(),
            vec![
                ("device.vendor.id".into(), "v".into()),
                ("device.serial".into(), "s".into()),
            ],
        )
        .unwrap();
        let sorted = SinkIdentity::new(
            "sink".into(),
            vec![
                ("device.serial".into(), "s".into()),
                ("device.vendor.id".into(), "v".into()),
            ],
        )
        .unwrap();
        assert_eq!(unsorted, sorted);
        let keys: Vec<&str> = unsorted
            .stable_properties()
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(keys, ["device.serial", "device.vendor.id"]);
    }

    #[test]
    fn compatible_with_requires_exact_name_and_saved_pairs() {
        let saved = identity("dock", &[("device.serial", "s1"), ("device.bus", "usb")]);
        let observed_superset = identity(
            "dock",
            &[
                ("device.serial", "s1"),
                ("device.bus", "usb"),
                ("device.vendor.id", "v"),
            ],
        );
        assert!(saved.compatible_with(&observed_superset));

        let missing_property = identity("dock", &[("device.serial", "s1")]);
        assert!(!saved.compatible_with(&missing_property));

        let changed_value = identity(
            "dock",
            &[("device.serial", "s1"), ("device.bus", "firewire")],
        );
        assert!(!saved.compatible_with(&changed_value));

        let renamed = identity("other", &[("device.serial", "s1"), ("device.bus", "usb")]);
        assert!(!saved.compatible_with(&renamed));
    }

    #[test]
    fn live_target_rejects_the_invalid_pulse_index() {
        let serial = NonZeroU64::new(7).unwrap();
        assert!(matches!(
            LiveSinkTarget::new(identity("sink", &[]), serial, PULSE_INVALID_INDEX),
            Err(AudioError::InvalidSelection(_))
        ));
        let target = LiveSinkTarget::new(identity("sink", &[]), serial, 3).unwrap();
        assert_eq!(target.pulse_index, 3);
        assert_eq!(target.object_serial, serial);
    }

    #[test]
    fn revision_starts_at_one_and_advances_without_wrapping() {
        let revision = OutputRevision::first();
        assert_eq!(revision.get(), 1);
        let advanced = revision.next().next();
        assert_eq!(advanced.get(), 3);
        assert_eq!(revision.get(), 1);
    }

    #[test]
    #[should_panic(expected = "output revision exhausted")]
    fn revision_increment_is_checked_not_wrapping() {
        let _ = OutputRevision(NonZeroU64::MAX).next();
    }
}
