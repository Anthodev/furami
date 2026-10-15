//! Borrow only bounded public facts when comparing ordinary GUI polls.
use super::*;
use crate::domain::{
    capture::{AudioAvailability, AudioSilence},
    output::{OutputPlan, PersistentOutputChoice},
};

pub(crate) struct AudioProjection<'a> {
    selected: Option<&'a str>,
    route: Option<Route<'a>>,
    availability: Option<AudioCode>,
    epoch: Option<u64>,
    revision: Option<u64>,
    plan_revision: u64,
    plan_reason: Option<AudioCode>,
    helpers: HelperReport,
}
struct Route<'a> {
    source: &'a str,
    sink: &'a str,
    attempt: u64,
    generation: u64,
    epoch: u64,
    revision: u64,
    watch: u64,
    observation_epoch: u64,
}
impl Route<'_> {
    fn matches(&self, cached: &EffectiveRoute) -> bool {
        self.source == cached.source_technical_name
            && self.sink == cached.sink_technical_name
            && self.attempt == cached.attempt
            && self.generation == cached.generation
            && self.epoch == cached.audio_epoch
            && self.revision == cached.output_revision
            && self.watch == cached.watch
            && self.observation_epoch == cached.observation_epoch
    }
    fn into_report(self) -> EffectiveRoute {
        EffectiveRoute {
            source_technical_name: self.source.to_owned(),
            sink_technical_name: self.sink.to_owned(),
            attempt: self.attempt,
            generation: self.generation,
            audio_epoch: self.epoch,
            output_revision: self.revision,
            watch: self.watch,
            observation_epoch: self.observation_epoch,
        }
    }
}
impl<'a> AudioProjection<'a> {
    pub(crate) fn new(
        choice: &'a PersistentOutputChoice,
        plan: &OutputPlan,
        availability: Option<&'a AudioAvailability>,
        helpers: HelperReport,
    ) -> Self {
        let mut result = Self {
            selected: match choice {
                PersistentOutputChoice::Auto => None,
                PersistentOutputChoice::Manual(sink) => Some(bounded_str(sink.name(), 256)),
            },
            route: None,
            availability: None,
            epoch: None,
            revision: None,
            plan_revision: plan.revision().get(),
            plan_reason: plan.silence().map(output_silence),
            helpers,
        };
        let Some(availability) = availability else {
            return result;
        };
        result.availability = Some(match availability {
            AudioAvailability::Disabled => AudioCode::Disabled,
            AudioAvailability::Silent { reason } => match reason {
                AudioSilence::WaitingForSource(_) => AudioCode::WaitingForSource,
                AudioSilence::PendingRoute => AudioCode::PendingRoute,
                AudioSilence::Paused => AudioCode::Paused,
                AudioSilence::Failed(_) => AudioCode::Failed,
                AudioSilence::Output(reason) => output_silence(reason),
            },
            AudioAvailability::Opening { epoch } => {
                result.epoch = Some(epoch.get());
                AudioCode::Opening
            }
            AudioAvailability::Active { route } => {
                result.epoch = Some(route.epoch().get());
                result.revision = Some(route.output_revision().get());
                result.route = route.destination().map(|sink| Route {
                    source: bounded_str(route.source().name(), 256),
                    sink: bounded_str(sink.identity.name(), 256),
                    attempt: route.attempt().get(),
                    generation: route.generation().get(),
                    epoch: route.epoch().get(),
                    revision: route.output_revision().get(),
                    watch: route.watch().watch.get(),
                    observation_epoch: route.watch().epoch.get(),
                });
                AudioCode::Active
            }
            AudioAvailability::Switching { epoch, revision } => {
                result.epoch = Some(epoch.get());
                result.revision = Some(revision.get());
                AudioCode::Switching
            }
            AudioAvailability::Detaching { epoch } => {
                result.epoch = Some(epoch.get());
                AudioCode::Detaching
            }
            AudioAvailability::Blocked { epoch, .. } => {
                result.epoch = Some(epoch.get());
                AudioCode::Blocked
            }
        });
        result
    }
    pub(crate) fn matches(&self, cached: &AudioReport) -> bool {
        let selected = match (self.selected, &cached.selected_output) {
            (None, SelectedOutput::Auto) => true,
            (Some(name), SelectedOutput::Manual { technical_name }) => name == technical_name,
            _ => false,
        };
        let route = match (&self.route, &cached.effective_route) {
            (
                None,
                Observation::Unavailable {
                    reason: UnavailableReason::NotObserved,
                },
            ) => true,
            (Some(route), Observation::Observed { value }) => route.matches(value),
            _ => false,
        };
        let availability = match (self.availability, &cached.availability) {
            (
                None,
                Observation::Unavailable {
                    reason: UnavailableReason::NotObserved,
                },
            ) => true,
            (Some(code), Observation::Observed { value }) => code == *value,
            _ => false,
        };
        selected
            && route
            && availability
            && self.epoch == cached.audio_epoch
            && self.revision == cached.output_revision
            && self.plan_revision == cached.plan_output_revision
            && self.plan_reason == cached.plan_reason
            && self.helpers == cached.helpers
    }
    pub(crate) fn into_report(self) -> AudioReport {
        AudioReport {
            selected_output: self
                .selected
                .map(|name| SelectedOutput::Manual {
                    technical_name: name.to_owned(),
                })
                .unwrap_or(SelectedOutput::Auto),
            effective_route: self
                .route
                .map(|route| Observation::Observed {
                    value: route.into_report(),
                })
                .unwrap_or_default(),
            availability: self
                .availability
                .map(|value| Observation::Observed { value })
                .unwrap_or_default(),
            audio_epoch: self.epoch,
            output_revision: self.revision,
            plan_output_revision: self.plan_revision,
            plan_reason: self.plan_reason,
            helpers: self.helpers,
        }
    }
}
