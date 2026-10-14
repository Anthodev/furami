//! Responsibility: Verified source settings, exact identity matching and sticky collisions.
//! Allowed dependencies: domain value types; no discovery, I/O or admission authority.

use thiserror::Error;

use crate::domain::capture::{AudioSelection, DeviceIdentity};
use crate::domain::state::{AppliedSettings, DraftSettings};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileBinding {
    Serial,
    Topology,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceProfile {
    binding: ProfileBinding,
    settings: DraftSettings,
}

impl SourceProfile {
    /// Only the strict decoder constructs profiles from externally supplied settings.
    pub(crate) fn new(
        binding: ProfileBinding,
        settings: DraftSettings,
    ) -> Result<Self, ProfileValidationError> {
        if binding == ProfileBinding::Serial && settings.video.identity.serial().is_none() {
            return Err(ProfileValidationError::SerialWithoutSerial);
        }
        Ok(Self { binding, settings })
    }

    pub fn binding(&self) -> ProfileBinding {
        self.binding
    }

    pub fn settings(&self) -> &DraftSettings {
        &self.settings
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SerialGroup {
    vendor_id: u16,
    product_id: u16,
    serial: String,
}

impl SerialGroup {
    fn matches(&self, identity: &DeviceIdentity) -> bool {
        self.vendor_id == identity.vendor_id()
            && self.product_id == identity.product_id()
            && Some(self.serial.as_str()) == identity.serial()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceProfiles {
    entries: Vec<SourceProfile>,
    /// Collisions without a verified profile remain session-only. Existing
    /// Topology entries carry useful collision history across restarts.
    collisions: Vec<SerialGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ProfileValidationError {
    #[error("a serial-bound profile requires a USB serial")]
    SerialWithoutSerial,
    #[error("multiple profiles contain the same complete device identity")]
    DuplicateIdentity,
    #[error("multiple serial-bound profiles belong to the same USB serial group")]
    MultipleSerialProfiles,
    #[error("serial and topology bindings are mixed in the same USB serial group")]
    MixedBindings,
    #[error("last_source does not identify a profile in this document")]
    InvalidLastSource,
}

impl SourceProfiles {
    pub(crate) fn new(entries: Vec<SourceProfile>) -> Result<Self, ProfileValidationError> {
        let profiles = Self {
            entries,
            collisions: Vec::new(),
        };
        profiles.validate(None)?;
        Ok(profiles)
    }

    pub fn entries(&self) -> &[SourceProfile] {
        &self.entries
    }

    /// Observe physical USB devices, not their individual video-node rows.
    /// Repeated complete identities are aliases, not additional physical devices.
    /// Only collisions without a recorded profile need a new string allocation.
    pub fn observe<'a, I>(&mut self, physical: I)
    where
        I: Iterator<Item = &'a DeviceIdentity> + Clone,
    {
        for identity in physical.clone() {
            if identity.serial().is_some()
                && physical
                    .clone()
                    .any(|other| same_serial_group(identity, other) && identity != other)
            {
                self.restrict_group(identity);
            }
        }
    }

    pub fn lookup(&self, selected: &DeviceIdentity, serial_unique: bool) -> Option<&DraftSettings> {
        let binding = self.binding_for(selected, serial_unique);
        self.entries
            .iter()
            .find(|profile| match binding {
                ProfileBinding::Serial => {
                    profile.binding == ProfileBinding::Serial
                        && same_serial_group(&profile.settings.video.identity, selected)
                }
                ProfileBinding::Topology => profile.settings.video.identity == *selected,
            })
            .map(SourceProfile::settings)
    }

    pub fn binding_for(&self, selected: &DeviceIdentity, serial_unique: bool) -> ProfileBinding {
        if selected.serial().is_some() && serial_unique && !self.group_is_restricted(selected) {
            ProfileBinding::Serial
        } else {
            ProfileBinding::Topology
        }
    }

    /// The applied receipt is the settings authority. A binding captured before
    /// a subsequent collision may become more restrictive, never less so.
    pub fn upsert_verified(&mut self, applied: &AppliedSettings, binding: ProfileBinding) {
        self.upsert_profile(applied, binding, None);
    }

    /// Project a verified automatic video adaptation without making its
    /// session-only CLI audio override durable.
    pub(crate) fn upsert_verified_preserving_audio(
        &mut self,
        applied: &AppliedSettings,
        binding: ProfileBinding,
        audio: &AudioSelection,
    ) {
        self.upsert_profile(applied, binding, Some(audio));
    }

    fn upsert_profile(
        &mut self,
        applied: &AppliedSettings,
        binding: ProfileBinding,
        preserved_audio: Option<&AudioSelection>,
    ) {
        let settings = applied.settings();
        let identity = &settings.video.identity;
        let binding = if binding == ProfileBinding::Topology
            || identity.serial().is_none()
            || self.group_is_restricted(identity)
        {
            self.restrict_group(identity);
            ProfileBinding::Topology
        } else {
            ProfileBinding::Serial
        };
        let entry = self.entries.iter_mut().find(|profile| match binding {
            ProfileBinding::Serial => same_serial_group(&profile.settings.video.identity, identity),
            ProfileBinding::Topology => profile.settings.video.identity == *identity,
        });
        if let Some(profile) = entry {
            profile.binding = binding;
            profile.settings.video.clone_from(&settings.video);
            profile
                .settings
                .audio
                .clone_from(preserved_audio.unwrap_or(&settings.audio));
            profile.settings.filters.clone_from(&settings.filters);
            profile.settings.renderer.clone_from(&settings.renderer);
        } else {
            let settings = DraftSettings {
                video: settings.video.clone(),
                audio: preserved_audio.unwrap_or(&settings.audio).clone(),
                filters: settings.filters.clone(),
                renderer: settings.renderer.clone(),
            };
            self.entries.push(SourceProfile { binding, settings });
        }
        // Once a Topology profile exists its binding carries this history, so
        // keep only collisions that still have no verified profile.
        self.collisions.retain(|group| !group.matches(identity));
    }

    pub(crate) fn validate(
        &self,
        last_source: Option<&DeviceIdentity>,
    ) -> Result<(), ProfileValidationError> {
        for (index, profile) in self.entries.iter().enumerate() {
            let identity = &profile.settings.video.identity;
            if profile.binding == ProfileBinding::Serial && identity.serial().is_none() {
                return Err(ProfileValidationError::SerialWithoutSerial);
            }
            for other in &self.entries[..index] {
                if identity == &other.settings.video.identity {
                    return Err(ProfileValidationError::DuplicateIdentity);
                }
                if same_serial_group(identity, &other.settings.video.identity) {
                    match (profile.binding, other.binding) {
                        (ProfileBinding::Serial, ProfileBinding::Serial) => {
                            return Err(ProfileValidationError::MultipleSerialProfiles);
                        }
                        (ProfileBinding::Serial, ProfileBinding::Topology)
                        | (ProfileBinding::Topology, ProfileBinding::Serial) => {
                            return Err(ProfileValidationError::MixedBindings);
                        }
                        (ProfileBinding::Topology, ProfileBinding::Topology) => {}
                    }
                }
            }
        }
        if last_source.is_some_and(|identity| {
            !self
                .entries
                .iter()
                .any(|profile| &profile.settings.video.identity == identity)
        }) {
            return Err(ProfileValidationError::InvalidLastSource);
        }
        Ok(())
    }

    fn group_is_restricted(&self, identity: &DeviceIdentity) -> bool {
        self.collisions.iter().any(|group| group.matches(identity))
            || self.entries.iter().any(|profile| {
                profile.binding == ProfileBinding::Topology
                    && same_serial_group(&profile.settings.video.identity, identity)
            })
    }

    fn restrict_group(&mut self, identity: &DeviceIdentity) {
        let Some(serial) = identity.serial() else {
            return;
        };
        if !self
            .entries
            .iter()
            .any(|profile| same_serial_group(&profile.settings.video.identity, identity))
            && !self.collisions.iter().any(|group| group.matches(identity))
        {
            self.collisions.push(SerialGroup {
                vendor_id: identity.vendor_id(),
                product_id: identity.product_id(),
                serial: serial.to_owned(),
            });
        }
        for profile in &mut self.entries {
            if same_serial_group(&profile.settings.video.identity, identity) {
                profile.binding = ProfileBinding::Topology;
            }
        }
    }
}

fn same_serial_group(left: &DeviceIdentity, right: &DeviceIdentity) -> bool {
    left.serial().is_some()
        && left.vendor_id() == right.vendor_id()
        && left.product_id() == right.product_id()
        && left.serial() == right.serial()
}
