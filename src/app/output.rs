//! Pure output-routing policy: Auto/manual resolution, the session-lifetime
//! manual loss latch and the checked desired-output revision.
//!
//! Allowed dependencies: `domain` values only. No Pulse connection, no
//! catalog worker, no persistence: callers feed published catalog snapshots
//! in and send the resulting plan out. The manual latch is runtime-only; app
//! relaunch deliberately resets it while a present manual choice still opens.

use crate::domain::capture::AudioError;
use crate::domain::output::{
    EASYEFFECTS_SINK_NAME, LiveSinkTarget, OutputPlan, OutputRevision, OutputSilence,
    PersistentOutputChoice, SinkCatalog,
};

/// Desired-resolution outcome compared against the published plan. Target
/// borrows the observed snapshot so unchanged observations never clone.
enum Desired<'a> {
    Target(&'a LiveSinkTarget),
    Silence(OutputSilence),
}

/// Owns the persistent output intent plus runtime-only latch and live
/// resolution state. `select` is the only action that clears the manual loss
/// latch; observations never do.
#[derive(Debug, Clone)]
pub struct OutputPolicy {
    choice: PersistentOutputChoice,
    revision: OutputRevision,
    published: OutputPlan,
    /// Sticky loss latch for the selected manual output of this app run.
    manual_lost: bool,
    /// A target went live for the current choice epoch (since the last
    /// explicit `select`): absence or ambiguity from then on is a loss.
    live_this_choice: bool,
    /// Last successfully observed catalog, kept for immediate re-resolution
    /// on explicit selection.
    catalog: Option<SinkCatalog>,
    /// Diagnostic detail of the last failed observation, until the next
    /// successful one.
    catalog_error: Option<String>,
}

impl OutputPolicy {
    /// Starts at revision 1 with an honest unobserved-catalog silence; the
    /// first published snapshot resolves the real plan.
    pub fn new(choice: PersistentOutputChoice) -> Self {
        Self {
            choice,
            revision: OutputRevision::first(),
            published: OutputPlan::Silent {
                revision: OutputRevision::first(),
                reason: OutputSilence::CatalogUnavailable("output catalog not yet observed".into()),
            },
            manual_lost: false,
            live_this_choice: false,
            catalog: None,
            catalog_error: None,
        }
    }

    /// Explicit user action: always advances the revision (an identical
    /// reselection gets a fresh revision so a new move/admission is not
    /// mistaken for a stale completion), clears the previous manual latch and
    /// resets the choice-epoch liveness, then re-resolves the last observed
    /// catalog. Selecting an unavailable target stays silent.
    pub fn select(&mut self, choice: PersistentOutputChoice) {
        self.choice = choice;
        self.manual_lost = false;
        self.live_this_choice = false;
        self.revision = self.revision.next();
        self.refresh(true);
    }

    /// Consumes one catalog observation. A failed observation is explicit
    /// catalog unavailability: it never invents a target and never infers
    /// absence. `selected_target_removed` is the watcher's sticky removal
    /// record for the current choice epoch: once a manual target went live,
    /// a removal signal latches the loss even if the snapshot already lists
    /// the sink again (remove/reappear between publications) or the catalog
    /// is momentarily unobservable. Reobservations never clear the latch:
    /// gain/mute changes, pause/resume, capture recovery and catalog
    /// refreshes have no path into this state.
    pub fn observe(
        &mut self,
        catalog: Result<SinkCatalog, AudioError>,
        selected_target_removed: bool,
    ) {
        let latched = selected_target_removed
            && matches!(self.choice, PersistentOutputChoice::Manual(_))
            && self.live_this_choice
            && !self.manual_lost;
        if latched {
            self.manual_lost = true;
        }
        match catalog {
            Ok(catalog) => {
                self.catalog = Some(catalog);
                self.catalog_error = None;
            }
            Err(error) => {
                self.catalog = None;
                self.catalog_error = Some(error.to_string());
            }
        }
        let revision_before = self.revision;
        self.refresh(false);
        if latched && self.revision == revision_before {
            // The latch is itself a semantic update of the desired plan even
            // when the published silence reason is unchanged: exactly one
            // revision step, never two for a single observation.
            self.revision = self.revision.next();
            self.refresh(true);
        }
    }

    pub fn plan(&self) -> &OutputPlan {
        &self.published
    }

    pub fn revision(&self) -> OutputRevision {
        self.revision
    }

    pub fn choice(&self) -> &PersistentOutputChoice {
        &self.choice
    }

    /// Re-resolves the desired plan from the retained observation state and
    /// publishes it. The revision advances only when the desired target
    /// (identity, serial or index) or the silence reason actually changes;
    /// unchanged observations, description-only changes and coalesced dirty
    /// worlds collapse into the current plan. `force` (explicit selection)
    /// republishes the desire at the revision the selection already advanced
    /// instead of bumping a second time.
    fn refresh(&mut self, force: bool) {
        let catalog = self.catalog.take();
        let desired = if let Some(catalog) = catalog.as_ref() {
            self.resolve(catalog)
        } else {
            Desired::Silence(OutputSilence::CatalogUnavailable(
                self.catalog_error
                    .clone()
                    .unwrap_or_else(|| "output catalog not yet observed".into()),
            ))
        };
        match desired {
            Desired::Target(target) => {
                if force || self.published.target() != Some(target) {
                    if !force {
                        self.revision = self.revision.next();
                    }
                    self.published = OutputPlan::Target {
                        revision: self.revision,
                        target: target.clone(),
                    };
                }
                if matches!(self.choice, PersistentOutputChoice::Manual(_)) {
                    self.live_this_choice = true;
                }
            }
            Desired::Silence(reason) => {
                if force || self.published.silence() != Some(&reason) {
                    if !force {
                        self.revision = self.revision.next();
                    }
                    self.published = OutputPlan::Silent {
                        revision: self.revision,
                        reason,
                    };
                }
            }
        }
        self.catalog = catalog;
    }

    /// Resolves one successfully observed catalog. Manual: exactly one
    /// compatible observation (the eligibility flag does not gate an explicit
    /// choice; the owner's move and destination observation confirm the
    /// route), no fallback on absence, changed identity or ambiguity, and a
    /// returned identity after a latch stays silent behind
    /// `ManualRequiresAction`. Auto: exactly one eligible EasyEffects sink,
    /// else the server default resolved uniquely among eligible
    /// observations, else `NoAvailableOutput`; IDLE/SUSPENDED stay eligible
    /// and the removal flag never constrains Auto.
    fn resolve<'catalog>(&mut self, catalog: &'catalog SinkCatalog) -> Desired<'catalog> {
        match &self.choice {
            PersistentOutputChoice::Manual(identity) => {
                let mut compatible = catalog
                    .sinks
                    .iter()
                    .filter(|obs| identity.compatible_with(&obs.target.identity));
                match (compatible.next(), compatible.next()) {
                    (Some(obs), None) if !self.manual_lost => Desired::Target(&obs.target),
                    (Some(_), None) => Desired::Silence(OutputSilence::ManualRequiresAction),
                    _ => {
                        if self.live_this_choice {
                            self.manual_lost = true;
                        }
                        Desired::Silence(OutputSilence::ManualUnavailable)
                    }
                }
            }
            PersistentOutputChoice::Auto => {
                let mut effects = catalog.sinks.iter().filter(|obs| {
                    obs.eligible && obs.target.identity.name() == EASYEFFECTS_SINK_NAME
                });
                match (effects.next(), effects.next()) {
                    (Some(obs), None) => Desired::Target(&obs.target),
                    _ => match catalog.default_sink.as_ref() {
                        Some(default) => {
                            let mut defaults = catalog.sinks.iter().filter(|obs| {
                                obs.eligible && default.compatible_with(&obs.target.identity)
                            });
                            match (defaults.next(), defaults.next()) {
                                (Some(obs), None) => Desired::Target(&obs.target),
                                _ => Desired::Silence(OutputSilence::NoAvailableOutput),
                            }
                        }
                        None => Desired::Silence(OutputSilence::NoAvailableOutput),
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;
    use crate::domain::output::{SinkIdentity, SinkObservation};

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

    fn sink(name: &str, serial: u64, index: u32) -> LiveSinkTarget {
        LiveSinkTarget::new(
            identity(name, &[("device.serial", name)]),
            NonZeroU64::new(serial).unwrap(),
            index,
        )
        .unwrap()
    }

    fn observed(target: LiveSinkTarget, eligible: bool, description: &str) -> SinkObservation {
        SinkObservation {
            target,
            description: description.into(),
            eligible,
        }
    }

    fn effects(index: u32, serial: u64, eligible: bool) -> SinkObservation {
        observed(
            LiveSinkTarget::new(
                identity(EASYEFFECTS_SINK_NAME, &[]),
                NonZeroU64::new(serial).unwrap(),
                index,
            )
            .unwrap(),
            eligible,
            "EasyEffects Output (state is never identity)",
        )
    }

    fn catalog(
        revision: u64,
        sinks: Vec<SinkObservation>,
        default_sink: Option<&SinkIdentity>,
    ) -> SinkCatalog {
        SinkCatalog {
            revision,
            sinks,
            default_sink: default_sink.cloned(),
        }
    }

    fn default_identity() -> SinkIdentity {
        identity("speakers", &[("device.serial", "speakers")])
    }

    fn err() -> AudioError {
        AudioError::Unavailable("catalog connection lost".into())
    }

    fn silence_of(plan: &OutputPlan) -> &OutputSilence {
        plan.silence().expect("plan must be silent")
    }

    #[test]
    fn initial_plan_is_honest_unobserved_catalog_silence() {
        for choice in [
            PersistentOutputChoice::Auto,
            PersistentOutputChoice::Manual(identity("sink", &[])),
        ] {
            let policy = OutputPolicy::new(choice);
            assert_eq!(
                silence_of(policy.plan()),
                &OutputSilence::CatalogUnavailable("output catalog not yet observed".into())
            );
            assert_eq!(policy.revision(), OutputRevision::first());
            assert!(policy.plan().target().is_none());
        }
    }

    #[test]
    fn auto_prefers_the_single_eligible_effects_sink_over_the_default() {
        let default = default_identity();
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Auto);
        policy.observe(
            Ok(catalog(
                1,
                vec![
                    effects(5, 100, true),
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                ],
                Some(&default),
            )),
            false,
        );
        let target = policy.plan().target().expect("effects sink must resolve");
        assert_eq!(target.identity.name(), EASYEFFECTS_SINK_NAME);
        assert_eq!(target.pulse_index, 5);
        assert_eq!(target.object_serial.get(), 100);
        assert_eq!(policy.revision().get(), 2);
    }

    #[test]
    fn auto_skips_an_ineligible_effects_sink() {
        let default = default_identity();
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Auto);
        policy.observe(
            Ok(catalog(
                1,
                vec![
                    effects(5, 100, false),
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                ],
                Some(&default),
            )),
            false,
        );
        let target = policy.plan().target().expect("default must resolve");
        assert_eq!(target.identity.name(), "speakers");
    }

    #[test]
    fn auto_with_two_eligible_effects_sinks_falls_back_to_the_unique_default() {
        let default = default_identity();
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Auto);
        policy.observe(
            Ok(catalog(
                1,
                vec![
                    effects(5, 100, true),
                    effects(6, 101, true),
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                ],
                Some(&default),
            )),
            false,
        );
        assert_eq!(
            policy.plan().target().expect("default").identity.name(),
            "speakers"
        );
    }

    #[test]
    fn auto_reresolves_when_the_default_changes() {
        let first = default_identity();
        let second = identity("headset", &[("device.serial", "headset")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Auto);
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                Some(&first),
            )),
            false,
        );
        assert_eq!(policy.plan().target().unwrap().identity.name(), "speakers");
        let before = policy.revision();
        policy.observe(
            Ok(catalog(
                2,
                vec![
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                    observed(sink("headset", 300, 10), true, "Headset"),
                ],
                Some(&second),
            )),
            false,
        );
        assert_eq!(policy.plan().target().unwrap().identity.name(), "headset");
        assert_eq!(policy.revision().get(), before.get() + 1);
    }

    #[test]
    fn auto_without_a_resolvable_candidate_is_explicit_silence() {
        let default = default_identity();
        let ambiguous = default_identity();
        let cases = vec![
            // No sinks at all.
            catalog(1, vec![], Some(&default)),
            // Default known but absent from the catalog.
            catalog(
                1,
                vec![observed(sink("headset", 300, 10), true, "Headset")],
                Some(&default),
            ),
            // Default identity matches two observations: ambiguity is silence.
            catalog(
                1,
                vec![
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                    observed(sink("speakers", 201, 10), true, "Speakers (2)"),
                ],
                Some(&ambiguous),
            ),
            // Default matches only an ineligible observation.
            catalog(
                1,
                vec![observed(sink("speakers", 200, 9), false, "Speakers")],
                Some(&default),
            ),
            // No server default published; no effects sink either.
            catalog(
                1,
                vec![observed(sink("headset", 300, 10), true, "Headset")],
                None,
            ),
        ];
        for world in cases {
            let mut policy = OutputPolicy::new(PersistentOutputChoice::Auto);
            policy.observe(Ok(world), false);
            assert_eq!(
                silence_of(policy.plan()),
                &OutputSilence::NoAvailableOutput,
                "expected explicit silence"
            );
        }
    }

    #[test]
    fn eligible_state_free_observations_are_routable_regardless_of_runtime_state() {
        // IDLE/SUSPENDED is not absence: eligibility is the only gate and the
        // policy never inspects a state string.
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Auto);
        policy.observe(
            Ok(catalog(
                1,
                vec![
                    effects(5, 100, true),
                    observed(sink("speakers", 200, 9), true, "Speakers (suspended)"),
                ],
                Some(&default_identity()),
            )),
            false,
        );
        assert_eq!(
            policy
                .plan()
                .target()
                .expect("eligible candidate")
                .identity
                .name(),
            EASYEFFECTS_SINK_NAME
        );
    }

    #[test]
    fn catalog_failure_is_explicit_silence_and_recovery_reresolves() {
        let default = default_identity();
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Auto);
        policy.observe(Err(err()), false);
        assert!(matches!(
            silence_of(policy.plan()),
            OutputSilence::CatalogUnavailable(_)
        ));
        let failed = policy.revision();
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                Some(&default),
            )),
            false,
        );
        assert_eq!(policy.plan().target().unwrap().identity.name(), "speakers");
        assert_eq!(policy.revision().get(), failed.get() + 1);
    }

    #[test]
    fn manual_selection_takes_priority_over_an_eligible_effects_sink() {
        let default = default_identity();
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(
                1,
                vec![
                    effects(5, 100, true),
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                ],
                Some(&default),
            )),
            false,
        );
        assert_eq!(policy.plan().target().unwrap().pulse_index, 9);
    }

    #[test]
    fn manual_absence_changed_identity_and_ambiguity_never_fall_back() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        // Same live name, incompatible saved property: not a same-name
        // substitution.
        let changed = SinkObservation {
            target: LiveSinkTarget::new(
                identity("speakers", &[("device.serial", "rewired")]),
                NonZeroU64::new(999).unwrap(),
                11,
            )
            .unwrap(),
            description: "Speakers (changed)".into(),
            eligible: true,
        };
        let worlds = vec![
            // Absent.
            catalog(1, vec![effects(5, 100, true)], None),
            // Same name, incompatible stable properties.
            catalog(1, vec![effects(5, 100, true), changed], None),
            // Two compatible observations: ambiguity is unavailable.
            catalog(
                1,
                vec![
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                    observed(sink("speakers", 201, 10), true, "Speakers (2)"),
                ],
                None,
            ),
        ];
        for world in worlds {
            let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual.clone()));
            policy.observe(Ok(world), false);
            assert_eq!(
                silence_of(policy.plan()),
                &OutputSilence::ManualUnavailable,
                "expected unavailable, never a fallback"
            );
        }
    }

    #[test]
    fn manual_selected_before_it_appears_does_not_latch_and_opens_on_return() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(Ok(catalog(1, vec![], None)), false);
        assert_eq!(silence_of(policy.plan()), &OutputSilence::ManualUnavailable);
        policy.observe(
            Ok(catalog(
                2,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            false,
        );
        assert_eq!(policy.plan().target().unwrap().pulse_index, 9);
    }

    #[test]
    fn app_launch_with_present_manual_opens_immediately() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(
                1,
                vec![
                    effects(5, 100, true),
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                ],
                Some(&default_identity()),
            )),
            false,
        );
        assert_eq!(policy.plan().target().unwrap().pulse_index, 9);
        assert_eq!(policy.revision().get(), 2);
    }

    #[test]
    fn sticky_removal_flag_latches_even_when_the_sink_is_still_listed() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            false,
        );
        assert!(policy.plan().target().is_some());
        let live_revision = policy.revision();
        // Remove/reappear completed between publications: the flag latches
        // although this snapshot lists the sink.
        policy.observe(
            Ok(catalog(
                2,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            true,
        );
        assert_eq!(
            silence_of(policy.plan()),
            &OutputSilence::ManualRequiresAction
        );
        assert_eq!(policy.revision().get(), live_revision.get() + 1);
    }

    #[test]
    fn manual_return_after_loss_keeps_silence_until_explicit_reselection() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            false,
        );
        let live_revision = policy.revision();
        // Loss observed by absence.
        policy.observe(Ok(catalog(2, vec![], None)), false);
        assert_eq!(silence_of(policy.plan()), &OutputSilence::ManualUnavailable);
        assert_eq!(policy.revision().get(), live_revision.get() + 1);
        // Same stable identity returns with a new serial: catalog/UI may see
        // it, the plan must not publish it.
        policy.observe(
            Ok(catalog(
                3,
                vec![observed(sink("speakers", 500, 12), true, "Speakers")],
                None,
            )),
            false,
        );
        assert_eq!(
            silence_of(policy.plan()),
            &OutputSilence::ManualRequiresAction
        );
        assert!(policy.plan().target().is_none());
        // Explicit reselection of the same identity clears the latch and
        // publishes the new serial at a fresh revision.
        policy.select(PersistentOutputChoice::Manual(identity(
            "speakers",
            &[("device.serial", "speakers")],
        )));
        let target = policy.plan().target().expect("explicit reselect opens");
        assert_eq!(target.object_serial.get(), 500);
        assert_eq!(target.pulse_index, 12);
        assert!(policy.revision().get() > live_revision.get() + 2);
    }

    #[test]
    fn explicit_reselect_of_the_same_identity_clears_the_flag_latch() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            false,
        );
        policy.observe(
            Ok(catalog(
                2,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            true,
        );
        assert_eq!(
            silence_of(policy.plan()),
            &OutputSilence::ManualRequiresAction
        );
        policy.select(PersistentOutputChoice::Manual(identity(
            "speakers",
            &[("device.serial", "speakers")],
        )));
        assert_eq!(policy.plan().target().unwrap().pulse_index, 9);
    }

    #[test]
    fn observations_after_a_latch_never_clear_it() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            false,
        );
        policy.observe(Ok(catalog(2, vec![], None)), false);
        assert_eq!(silence_of(policy.plan()), &OutputSilence::ManualUnavailable);
        // Same stable identity returns with a new serial: the reason
        // transitions to requires-action exactly once.
        policy.observe(
            Ok(catalog(
                3,
                vec![observed(sink("speakers", 500, 12), true, "Speakers")],
                None,
            )),
            false,
        );
        assert_eq!(
            silence_of(policy.plan()),
            &OutputSilence::ManualRequiresAction
        );
        let latched_revision = policy.revision();
        // Gain/mute/pause/resume and capture recovery have no policy input;
        // the only observable analogue is further unchanged catalog traffic
        // with churning catalog revisions.
        for catalog_revision in 4..=6u64 {
            policy.observe(
                Ok(catalog(
                    catalog_revision,
                    vec![observed(sink("speakers", 500, 12), true, "Speakers")],
                    None,
                )),
                false,
            );
            assert_eq!(
                silence_of(policy.plan()),
                &OutputSilence::ManualRequiresAction
            );
        }
        assert_eq!(policy.revision(), latched_revision);
    }

    #[test]
    fn switching_to_auto_after_a_manual_loss_resolves_auto_normally() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            false,
        );
        policy.observe(Ok(catalog(2, vec![], None)), false);
        assert_eq!(silence_of(policy.plan()), &OutputSilence::ManualUnavailable);
        // The selected identity actually returns: the latch keeps the plan
        // silent even though the catalog lists the sink again.
        policy.observe(
            Ok(catalog(
                3,
                vec![
                    effects(5, 100, true),
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                ],
                Some(&default_identity()),
            )),
            false,
        );
        assert_eq!(
            silence_of(policy.plan()),
            &OutputSilence::ManualRequiresAction
        );
        policy.select(PersistentOutputChoice::Auto);
        assert_eq!(
            policy.plan().target().unwrap().identity.name(),
            EASYEFFECTS_SINK_NAME
        );
        assert!(matches!(policy.choice(), PersistentOutputChoice::Auto));
    }

    #[test]
    fn revisions_advance_only_on_meaningful_desired_changes() {
        let default = default_identity();
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Auto);
        let world = || {
            catalog(
                1,
                vec![
                    effects(5, 100, true),
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                ],
                Some(&default),
            )
        };
        policy.observe(Ok(world()), false);
        assert_eq!(policy.plan().target().unwrap().pulse_index, 5);
        let stable = policy.revision();
        // Unchanged world: no bump.
        policy.observe(Ok(world()), false);
        assert_eq!(policy.revision(), stable);
        // Display-only change: no bump.
        policy.observe(
            Ok(catalog(
                2,
                vec![
                    effects(5, 100, true),
                    observed(sink("speakers", 200, 9), true, "Speakers (renamed)"),
                ],
                Some(&default),
            )),
            false,
        );
        assert_eq!(policy.revision(), stable);
        // Recreated effects sink with a new serial: desired target changed,
        // one bump and the new serial is published (no removal evidence).
        policy.observe(
            Ok(catalog(
                3,
                vec![
                    effects(5, 150, true),
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                ],
                Some(&default),
            )),
            false,
        );
        assert_eq!(policy.plan().target().unwrap().object_serial.get(), 150);
        assert_eq!(policy.revision().get(), stable.get() + 1);
        let after_serial = policy.revision();
        // Eligibility flip changes the Auto resolution: one bump.
        policy.observe(
            Ok(catalog(
                4,
                vec![
                    effects(5, 150, false),
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                ],
                Some(&default),
            )),
            false,
        );
        assert_eq!(policy.plan().target().unwrap().identity.name(), "speakers");
        assert_eq!(policy.revision().get(), after_serial.get() + 1);
    }

    #[test]
    fn explicit_selection_always_advances_the_revision_even_when_unchanged() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual.clone()));
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            false,
        );
        let before = policy.revision();
        policy.select(PersistentOutputChoice::Manual(manual));
        assert_eq!(policy.revision().get(), before.get() + 1);
        assert_eq!(policy.plan().target().unwrap().pulse_index, 9);
    }

    #[test]
    fn dirty_world_coalesces_into_one_revision_step() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            false,
        );
        let live_revision = policy.revision();
        // Removal and reappear with a new serial happened before this single
        // published snapshot: one observe, one bump, straight to requires-
        // action without ever publishing the returned serial.
        policy.observe(
            Ok(catalog(
                2,
                vec![observed(sink("speakers", 500, 12), true, "Speakers")],
                None,
            )),
            true,
        );
        assert_eq!(
            silence_of(policy.plan()),
            &OutputSilence::ManualRequiresAction
        );
        assert_eq!(policy.revision().get(), live_revision.get() + 1);
        assert!(policy.plan().target().is_none());
    }

    #[test]
    fn removal_flag_is_ignored_for_auto_and_without_a_live_target() {
        let default = default_identity();
        // Auto ignores the flag and follows the catalog.
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Auto);
        policy.observe(
            Ok(catalog(1, vec![effects(5, 100, true)], Some(&default))),
            false,
        );
        policy.observe(
            Ok(catalog(
                2,
                vec![
                    effects(5, 100, true),
                    observed(sink("speakers", 200, 9), true, "Speakers"),
                ],
                Some(&identity("speakers", &[("device.serial", "speakers")])),
            )),
            true,
        );
        assert_eq!(
            policy.plan().target().unwrap().identity.name(),
            EASYEFFECTS_SINK_NAME
        );
        // A manual choice that never went live does not latch on the flag.
        let manual = identity("headphones", &[("device.serial", "headphones")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(1, vec![effects(5, 100, true)], Some(&default))),
            true,
        );
        assert_eq!(silence_of(policy.plan()), &OutputSilence::ManualUnavailable);
    }

    #[test]
    fn removal_flag_during_catalog_outage_latches_with_one_revision_step() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            false,
        );
        assert!(policy.plan().target().is_some());
        // The catalog drops out; the plan is explicit silence.
        policy.observe(Err(err()), false);
        let after_outage = policy.revision();
        assert!(matches!(
            silence_of(policy.plan()),
            OutputSilence::CatalogUnavailable(_)
        ));
        // The removal completes during the outage: the latch is a semantic
        // update even though the published reason is unchanged, exactly one
        // revision step for the whole observation.
        policy.observe(Err(err()), true);
        assert!(matches!(
            silence_of(policy.plan()),
            OutputSilence::CatalogUnavailable(_)
        ));
        assert_eq!(policy.revision().get(), after_outage.get() + 1);
        // The catalog returns with the same identity and a new serial: the
        // latched loss requires action, never an automatic republish.
        policy.observe(
            Ok(catalog(
                2,
                vec![observed(sink("speakers", 500, 12), true, "Speakers")],
                None,
            )),
            false,
        );
        assert_eq!(
            silence_of(policy.plan()),
            &OutputSilence::ManualRequiresAction
        );
        assert!(policy.plan().target().is_none());
    }

    #[test]
    fn explicit_selection_after_a_catalog_failure_stays_silent() {
        let manual = identity("speakers", &[("device.serial", "speakers")]);
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Manual(manual));
        policy.observe(
            Ok(catalog(
                1,
                vec![observed(sink("speakers", 200, 9), true, "Speakers")],
                None,
            )),
            false,
        );
        assert!(policy.plan().target().is_some());
        policy.observe(Err(err()), false);
        assert!(policy.plan().target().is_none());
        // An explicit selection must not resurrect the stale cached target:
        // without a fresh observation there is nothing routable to resolve.
        policy.select(PersistentOutputChoice::Manual(identity(
            "speakers",
            &[("device.serial", "speakers")],
        )));
        assert!(policy.plan().target().is_none());
        assert!(matches!(
            silence_of(policy.plan()),
            OutputSilence::CatalogUnavailable(_)
        ));
    }

    #[test]
    fn auto_resolves_a_working_effects_sink_without_a_published_default() {
        let mut policy = OutputPolicy::new(PersistentOutputChoice::Auto);
        policy.observe(Ok(catalog(1, vec![effects(5, 100, true)], None)), false);
        let target = policy.plan().target().expect("effects sink is routable");
        assert_eq!(target.identity.name(), EASYEFFECTS_SINK_NAME);
        assert_eq!(target.pulse_index, 5);
    }
}
