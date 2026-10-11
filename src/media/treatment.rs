//! Owner-only aggregate transaction. Native replies, never requested values,
//! authorize the renderer proof; vf owns its independent observation window.
use super::*;
use crate::domain::{
    failure::{
        RendererErrorKind, RendererFailure, RendererOperation, TreatmentConfirmationFailure,
        TreatmentErrorKind, TreatmentFailure, TreatmentFailureDisposition,
    },
    render::RendererProperty,
    state::{
        FilterEvidence, FilterTreatmentProof, RendererConfirmation, TreatmentConfirmation,
        TreatmentPass,
    },
};
use crate::media::{
    controller::PreparedTreatments,
    render_catalog::{NativeRendererValue, native_name},
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Snapshot,
    Write,
    Set,
    Readback,
    Verify,
    Filter,
    Final,
}

// Each pass needs at most 48 renderer/pause requests and 201 vf samples.
// Reserve candidate and its sole restoration before the first native write.
const PASS_REQUESTS: u64 = 249;

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct RendererFixtureFault {
    pub candidate: RendererProperty,
    pub restore: Option<RendererProperty>,
}

pub(super) struct TreatmentObservation {
    id: u64,
    key: TreatmentAttemptKey,
    target: PreparedTreatments,
    playback: InitialPlayback,
    phase: Phase,
    index: usize,
    pending: bool,
    different: [bool; 9],
    mutated: bool,
    uncertain_set: bool,
    unattributed_error: bool,
    vf_changed: bool,
    filters: Option<FilterTreatmentProof>,
    admitted: Instant,
    request: u64,
    request_end: u64,
    failure: Option<TreatmentFailure>,
    failure_without_drain: bool,
}

pub(super) struct TreatmentRestore {
    apply: crate::domain::state::ApplyId,
    prior: PreparedTreatments,
    origin: FilterEvidence,
    playback: InitialPlayback,
    vf_touched: bool,
    snapshot_verified: bool,
    restore_admitted: bool,
    request: u64,
    request_end: u64,
}

// Pinned mpv choice_get encodes empty scaler inheritance as INT64 0 and
// dither=no as FLAG false. These aliases are property-specific, not coercions.
fn native_choice(property: RendererProperty, observed: Option<&NodeValue>) -> Option<&CStr> {
    match (property, observed) {
        (_, Some(NodeValue::String(value))) if value.to_str().is_ok() => Some(value.as_c_str()),
        (RendererProperty::Cscale | RendererProperty::Dscale, Some(NodeValue::Int(0))) => Some(c""),
        (RendererProperty::Dither, Some(NodeValue::Flag(false))) => Some(c"no"),
        _ => None,
    }
}

fn matches_native(
    property: RendererProperty,
    expected: NativeRendererValue,
    observed: Option<&NodeValue>,
) -> bool {
    match (expected, observed) {
        (NativeRendererValue::Choice(expected), observed) => {
            native_choice(property, observed) == Some(expected)
        }
        (NativeRendererValue::Flag(expected), Some(NodeValue::Flag(actual))) => expected == *actual,
        (NativeRendererValue::Int(expected), Some(NodeValue::Int(actual))) => expected == *actual,
        (NativeRendererValue::Float(expected), Some(NodeValue::Double(actual))) => {
            actual.is_finite() && expected == *actual
        }
        _ => false,
    }
}

fn valid_native_type(
    property: RendererProperty,
    expected: NativeRendererValue,
    observed: Option<&NodeValue>,
) -> bool {
    (matches!(expected, NativeRendererValue::Choice(_))
        && native_choice(property, observed).is_some())
        || matches!(
            (expected, observed),
            (NativeRendererValue::Flag(_), Some(NodeValue::Flag(_)))
                | (NativeRendererValue::Int(_), Some(NodeValue::Int(_)))
        )
        || matches!(
            (expected, observed),
            (NativeRendererValue::Float(_), Some(NodeValue::Double(value))) if value.is_finite()
        )
}

impl TreatmentObservation {
    fn disposition(&self) -> TreatmentFailureDisposition {
        if self.uncertain_set {
            TreatmentFailureDisposition::RestoreFreshOwner
        } else if self.mutated {
            TreatmentFailureDisposition::RestoreSameOwner
        } else {
            TreatmentFailureDisposition::Untouched
        }
    }
    fn renderer_failure(&mut self, property: RendererProperty, kind: RendererErrorKind) {
        let disposition = match kind {
            RendererErrorKind::IncumbentMismatch => TreatmentFailureDisposition::RestoreFreshOwner,
            RendererErrorKind::ReadbackMismatch => TreatmentFailureDisposition::RestoreSameOwner,
            _ => self.disposition(),
        };
        self.failure.get_or_insert_with(|| TreatmentFailure {
            key: Some(self.key),
            kind: TreatmentErrorKind::Renderer(RendererFailure {
                property,
                diagnostic: format!("{}: {kind:?}", native_name(property).to_string_lossy()),
                kind,
            }),
            disposition,
        });
    }
    fn lost(&mut self, reason: TreatmentConfirmationFailure) {
        if let Some(failure) = &mut self.failure {
            failure.disposition = TreatmentFailureDisposition::RestoreFreshOwner;
        } else {
            self.failure = Some(TreatmentFailure {
                key: Some(self.key),
                kind: TreatmentErrorKind::Unconfirmed { reason },
                disposition: TreatmentFailureDisposition::RestoreFreshOwner,
            });
        }
        self.failure_without_drain = true;
    }
    pub(super) fn next_request(&mut self) -> Result<u64, MediaError> {
        if self.request == self.request_end {
            return Err(MediaError::new(
                "request_exhausted",
                "reserved treatment request identifiers exhausted",
            ));
        }
        self.request += 1;
        Ok(self.request)
    }
    pub(super) fn progress_request(&mut self, key: TreatmentAttemptKey) -> Result<u64, MediaError> {
        if key != self.key {
            return Err(MediaError::new(
                "request_key",
                "vf sample does not match the reserved treatment",
            ));
        }
        self.next_request()
    }
}

impl Handle {
    fn renderer_get(&self, id: u64, name: &CStr) -> Result<(), i32> {
        // SAFETY: Owner-only handle, exact reserved cookie, NODE result copied
        // by the event reader before its next native wait.
        let result =
            unsafe { (self.functions.get_property_async)(self.raw.as_ptr(), id, name.as_ptr(), 6) };
        if result < 0 { Err(result) } else { Ok(()) }
    }
    /// Closed choices and stack scalars are copied by libmpv during the call.
    fn renderer_value(
        &self,
        id: Option<u64>,
        property: RendererProperty,
        value: NativeRendererValue,
    ) -> Result<(), i32> {
        let name = native_name(property);
        let submit = |format, data| unsafe {
            if let Some(id) = id {
                (self.functions.set_property_async)(
                    self.raw.as_ptr(),
                    id,
                    name.as_ptr(),
                    format,
                    data,
                )
            } else {
                (self.functions.set_option_value)(self.raw.as_ptr(), name.as_ptr(), format, data)
            }
        };
        let result = match value {
            NativeRendererValue::Choice(value) => {
                let mut pointer = value.as_ptr();
                submit(1, ptr::from_mut(&mut pointer).cast())
            }
            NativeRendererValue::Flag(value) => {
                let mut value = c_int::from(value);
                submit(3, ptr::from_mut(&mut value).cast())
            }
            NativeRendererValue::Int(mut value) => submit(4, ptr::from_mut(&mut value).cast()),
            NativeRendererValue::Float(mut value) => submit(5, ptr::from_mut(&mut value).cast()),
        };
        if result < 0 { Err(result) } else { Ok(()) }
    }
}

impl MpvBackend {
    fn get_renderer_async(
        &mut self,
        key: TreatmentAttemptKey,
        id: u64,
        name: &'static CStr,
    ) -> Result<Result<(), i32>, MediaError> {
        tracing::info!(key = ?key, request = id, property = %name.to_string_lossy(), "renderer_get_submit");
        #[cfg(test)]
        {
            self.native_record(serde_json::json!({"event":"renderer_get","request":id,"property":name.to_str().expect("static native name")}));
            if let Some(requests) = &mut self.property_requests {
                requests.push((id, name, None));
                return Ok(Ok(()));
            }
        }
        Ok(self.handle()?.renderer_get(id, name))
    }

    fn set_renderer_async(
        &mut self,
        key: TreatmentAttemptKey,
        id: u64,
        property: RendererProperty,
        value: NativeRendererValue,
    ) -> Result<Result<(), i32>, MediaError> {
        #[cfg(test)]
        let value = if self.renderer_fixture_fault.is_some_and(|fault| {
            (key.pass == TreatmentPass::Candidate && property == fault.candidate)
                || (key.pass == TreatmentPass::Restore && fault.restore == Some(property))
        }) {
            assert!(
                matches!(value, NativeRendererValue::Choice(_)),
                "native fault requires a real choice property"
            );
            NativeRendererValue::Choice(c"furami-invalid-renderer-fixture")
        } else {
            value
        };
        tracing::info!(key = ?key, request = id, property = %native_name(property).to_string_lossy(), value = ?value, "renderer_set_submit");
        #[cfg(test)]
        {
            self.native_record(serde_json::json!({"event":"renderer_set","key":key,"request":id,"property":native_name(property).to_str().expect("static name"),"value":format!("{value:?}")}));
            if let Some(requests) = &mut self.property_requests {
                let node = match value {
                    NativeRendererValue::Choice(value) => NodeValue::String(value.to_owned()),
                    NativeRendererValue::Flag(value) => NodeValue::Flag(value),
                    NativeRendererValue::Int(value) => NodeValue::Int(value),
                    NativeRendererValue::Float(value) => NodeValue::Double(value),
                };
                requests.push((id, native_name(property), Some(node)));
                return Ok(Ok(()));
            }
        }
        Ok(self.handle()?.renderer_value(Some(id), property, value))
    }
    pub(super) fn install_renderer(&self, handle: &Handle) -> Result<(), MediaError> {
        let Some((_, treatments)) = &self.initial_treatments else {
            return Err(MediaError::new(
                "treatment_options",
                "missing prepared initial treatments",
            ));
        };
        for &(property, value) in treatments.renderer.native() {
            handle
                .renderer_value(None, property, value)
                .map_err(|error| {
                    MediaError::new(
                        "renderer_option",
                        format!(
                            "{}: mpv error={error}",
                            native_name(property).to_string_lossy()
                        ),
                    )
                })?;
        }
        Ok(())
    }

    pub(super) fn begin_treatment(
        &mut self,
        id: u64,
        key: TreatmentAttemptKey,
        target: PreparedTreatments,
    ) -> Result<(), MediaError> {
        if self.treatment_observation.is_some() || self.filter_observation.is_some() {
            return Err(MediaError::new(
                "treatment_admission",
                "one treatment transaction already pending",
            ));
        }
        let playback = if self.paused {
            InitialPlayback::Paused
        } else {
            InitialPlayback::Live
        };
        let (request, request_end, origin, vf_changed) = if key.pass == TreatmentPass::Restore {
            let retained = self
                .treatment_restore
                .as_mut()
                .filter(|retained| {
                    retained.apply == key.apply
                        && retained.snapshot_verified
                        && !retained.restore_admitted
                })
                .ok_or_else(|| {
                    MediaError::new(
                        "treatment_restore",
                        "no verified snapshot for this treatment admission",
                    )
                })?;
            if retained.prior != target || retained.playback != playback {
                return Err(MediaError::new(
                    "treatment_restore",
                    "restore does not match retained complete prior",
                ));
            }
            retained.restore_admitted = true;
            (
                retained.request,
                retained.request_end,
                Some(retained.origin),
                retained.vf_touched,
            )
        } else {
            let prior = self.landed_treatments.as_ref();
            if key.pass != TreatmentPass::Open && prior.is_none() {
                return Err(MediaError::new(
                    "treatment_admission",
                    "incumbent treatment proof unavailable",
                ));
            }
            let vf_changed = if key.pass == TreatmentPass::Open {
                playback == InitialPlayback::Live
            } else {
                prior.is_none_or(|(prior, _)| prior.filters.vf != target.filters.vf)
            };
            let origin = prior.map(|(_, confirmation)| confirmation.filters().origin());
            // Reserve both candidate and the sole same-owner restore before writing.
            // Renderer/pause and the independent bounded vf observer share this
            // finite reservation; incidental metadata retains its existing path.
            let request = self.last_internal;
            let end = request.checked_add(PASS_REQUESTS * 2).ok_or_else(|| {
                MediaError::new(
                    "request_exhausted",
                    "cannot reserve treatment and restore requests",
                )
            })?;
            self.last_internal = end;
            if let Some((prior, confirmation)) = prior {
                self.treatment_restore = Some(TreatmentRestore {
                    apply: key.apply,
                    prior: prior.clone(),
                    origin: confirmation.filters().origin(),
                    playback,
                    vf_touched: false,
                    snapshot_verified: false,
                    restore_admitted: false,
                    request: request + PASS_REQUESTS,
                    request_end: end,
                });
            }
            (request, request + PASS_REQUESTS, origin, vf_changed)
        };
        if vf_changed && playback == InitialPlayback::Paused {
            return Err(MediaError::new(
                "treatment_admission",
                "paused vf changes require a fresh Open",
            ));
        }
        let filters = if key.pass == TreatmentPass::Open && playback == InitialPlayback::Paused {
            Some(FilterTreatmentProof::PreparedPaused { key })
        } else if !vf_changed {
            origin.map(|origin| FilterTreatmentProof::Preserved { origin })
        } else {
            None
        };
        let failure = self.filters_poisoned.then(|| TreatmentFailure {
            key: Some(key),
            kind: TreatmentErrorKind::Filter(FilterFailure {
                kind: FilterErrorKind::Unconfirmed {
                    reason: FilterConfirmationFailure::BackendUnavailable,
                },
                requires_fresh_owner: true,
                attributed_ordinal: None,
                diagnostics: self.filter_diagnostics(
                    key,
                    &target.filters.entries,
                    self.log_sequence,
                    self.dropped_context,
                ),
            }),
            disposition: TreatmentFailureDisposition::RestoreFreshOwner,
        });
        let observation = TreatmentObservation {
            id,
            key,
            target,
            playback,
            phase: if key.pass == TreatmentPass::Open {
                Phase::Verify
            } else {
                Phase::Snapshot
            },
            index: 0,
            pending: false,
            different: [false; 9],
            mutated: false,
            uncertain_set: false,
            vf_changed,
            unattributed_error: false,
            filters,
            admitted: Instant::now(),
            request,
            request_end,
            failure,
            failure_without_drain: self.filters_poisoned,
        };
        tracing::info!(key = ?key, request = id, playback = ?playback, vf_changed, renderer = ?observation.target.renderer.requested(), "treatment_admitted");
        #[cfg(test)]
        self.native_record(serde_json::json!({
            "event":"treatment_admission", "key":key, "request":id,
            "playback":playback, "vf_changed":vf_changed,
            "renderer":observation.target.renderer.requested()
        }));
        self.treatment_observation = Some(observation);
        Ok(())
    }

    pub(super) fn treatment_lost(&mut self, reason: TreatmentConfirmationFailure) {
        if let Some(observation) = &mut self.treatment_observation {
            observation.lost(reason);
        }
    }

    pub(super) fn treatment_filter_revoked(&mut self, failure: &FilterFailure) {
        if let Some(observation) = &mut self.treatment_observation {
            observation.failure.get_or_insert_with(|| TreatmentFailure {
                key: Some(observation.key),
                kind: TreatmentErrorKind::Filter(failure.clone()),
                disposition: TreatmentFailureDisposition::RestoreFreshOwner,
            });
            observation.lost(TreatmentConfirmationFailure::BackendUnavailable);
        }
    }
    pub(super) fn renderer_log_error(&mut self) {
        if let Some(observation) = &mut self.treatment_observation {
            observation.unattributed_error = true;
        }
    }

    pub(super) fn invalidate_treatment_read(&mut self, id: u64) -> bool {
        let Some(index) = self.pending.iter().position(|(request, pending)| {
            *request == id
                && matches!(
                    pending,
                    Pending::RendererRead { .. } | Pending::RendererPause { .. }
                )
        }) else {
            return false;
        };
        let (_, pending) = self.pending.swap_remove(index);
        if let Some(observation) = &mut self.treatment_observation {
            observation.pending = false;
            match pending {
                Pending::RendererRead { property, .. } => {
                    observation.renderer_failure(property, RendererErrorKind::MalformedReadback);
                    observation
                        .failure
                        .as_mut()
                        .expect("malformed renderer read")
                        .disposition = TreatmentFailureDisposition::RestoreFreshOwner;
                    observation.failure_without_drain = true;
                }
                Pending::RendererPause { .. } => {
                    observation.lost(TreatmentConfirmationFailure::PlaybackChanged)
                }
                _ => unreachable!(),
            }
        }
        true
    }

    fn finish_treatment(&mut self) {
        let mut observation = self
            .treatment_observation
            .take()
            .expect("settled treatment");
        if self.filters_poisoned
            || self.native_evidence_lost
            || self
                .filter_health
                .as_ref()
                .is_some_and(|health| health.faulted)
        {
            observation.lost(if self.native_evidence_lost {
                TreatmentConfirmationFailure::EvidenceLost
            } else {
                TreatmentConfirmationFailure::BackendUnavailable
            });
        }
        if observation.failure.is_some() {
            if let Some(filter) = &mut self.filter_observation {
                filter
                    .failure
                    .get_or_insert(FilterConfirmationFailure::BackendUnavailable);
                filter.fresh = true;
                self.finish_filter();
            }
            self.filter_result = None;
        }
        self.pending.retain(|(_, pending)| !matches!(pending,
            Pending::RendererRead { key, .. } | Pending::RendererSet { key, .. } | Pending::RendererPause { key, .. }
            if *key == observation.key));
        let result = if let Some(failure) = observation.failure {
            Err(Box::new(failure))
        } else {
            let confirmation = TreatmentConfirmation::checked(
                observation.key,
                observation.playback,
                observation.vf_changed,
                observation.filters.expect("completed vf proof"),
                RendererConfirmation::checked(
                    observation.key,
                    observation.target.renderer.requested().clone(),
                ),
            );
            match confirmation {
                Ok(confirmation) => {
                    tracing::info!(
                        key = ?observation.key, playback = ?observation.playback,
                        requested = ?observation.target.renderer.requested(),
                        canonical_native = ?observation.target.renderer.native(),
                        filters = ?confirmation.filters(), "treatment_confirmed"
                    );
                    if self.filter_health.is_none() {
                        let origin = confirmation.filters().origin().key();
                        self.filter_health = Some(FilterHealth {
                            key: origin,
                            treatment_key: observation.key,
                            entries: observation.target.filters.entries.clone(),
                            marker: self.log_sequence,
                            dropped_at_begin: self.dropped_context,
                            faulted: false,
                        });
                    }
                    if let Some(health) = &mut self.filter_health {
                        health.treatment_key = observation.key;
                    }
                    self.landed_treatments = Some((observation.target, confirmation.clone()));
                    self.treatment_restore = None;
                    Ok(confirmation)
                }
                Err(reason) => Err(Box::new(TreatmentFailure {
                    key: Some(observation.key),
                    kind: TreatmentErrorKind::Unconfirmed { reason },
                    disposition: TreatmentFailureDisposition::RestoreFreshOwner,
                })),
            }
        };
        if result
            .as_ref()
            .is_err_and(|failure| failure.disposition == TreatmentFailureDisposition::Untouched)
            && self.landed_treatments.is_some()
            && !self.filters_poisoned
            && !self.native_evidence_lost
        {
            self.treatment_restore = None;
        }
        #[cfg(test)]
        self.native_record(
            serde_json::json!({"event":"treatment_result","key":observation.key,"result":result}),
        );
        self.emitted.push_back(BackendEvent::TreatmentResult {
            id: observation.id,
            key: observation.key,
            result,
        });
    }

    pub(super) fn treatment_tick(&mut self, now: Instant, clean: bool) -> Result<(), MediaError> {
        let Some(mut observation) = self.treatment_observation.take() else {
            return Ok(());
        };
        // A component deadline/evidence-loss negative is already settled by the
        // vf observer. Consume it without waiting for a NONE that may never come.
        if observation.phase == Phase::Filter {
            if let Some(result) = self.filter_result.take() {
                match result {
                    Ok(confirmation) if observation.failure.is_none() => {
                        observation.filters =
                            Some(FilterTreatmentProof::Reconfigured(confirmation));
                        observation.phase = Phase::Final;
                        observation.index = 0;
                        observation.admitted = now;
                    }
                    Ok(_) => {}
                    Err(failure) => {
                        let vf_untouched = matches!(
                            failure.kind,
                            FilterErrorKind::CommandRejected { .. }
                                | FilterErrorKind::CommandSubmission { .. }
                        ) && !failure.requires_fresh_owner;
                        if vf_untouched && let Some(retained) = &mut self.treatment_restore {
                            retained.vf_touched = false;
                        }
                        let disposition = if failure.requires_fresh_owner || !clean {
                            TreatmentFailureDisposition::RestoreFreshOwner
                        } else if observation.mutated || !vf_untouched {
                            TreatmentFailureDisposition::RestoreSameOwner
                        } else {
                            TreatmentFailureDisposition::Untouched
                        };
                        if observation.failure.as_ref().is_none_or(|current| {
                            matches!(current.kind, TreatmentErrorKind::Unconfirmed { .. })
                        }) {
                            observation.failure = Some(TreatmentFailure {
                                key: Some(observation.key),
                                kind: TreatmentErrorKind::Filter(*failure),
                                disposition,
                            });
                        }
                        observation.failure_without_drain |= !clean;
                    }
                }
            }
            let settled =
                observation.failure.is_some() && (clean || observation.failure_without_drain);
            self.treatment_observation = Some(observation);
            if settled {
                self.finish_treatment();
            }
            return Ok(());
        }
        if observation.phase != Phase::Filter
            && now.duration_since(observation.admitted) >= FilterConfirmation::DEADLINE
        {
            if observation.pending && observation.phase == Phase::Set {
                observation.uncertain_set = true;
            }
            if observation.index >= 9 {
                observation.lost(TreatmentConfirmationFailure::Deadline);
            } else {
                let property = observation.target.renderer.native()[observation.index.min(8)].0;
                let operation = match observation.phase {
                    Phase::Snapshot => RendererOperation::Snapshot,
                    Phase::Set => RendererOperation::Set,
                    _ => RendererOperation::Readback,
                };
                observation.renderer_failure(property, RendererErrorKind::Deadline { operation });
            }
            if !clean {
                observation.lost(TreatmentConfirmationFailure::EvidenceLost);
            }
        }
        if clean
            && observation.unattributed_error
            && !matches!(
                &observation.failure,
                Some(TreatmentFailure {
                    kind: TreatmentErrorKind::Renderer(RendererFailure {
                        kind: RendererErrorKind::Rejected { .. },
                        ..
                    }),
                    ..
                })
            )
        {
            observation.lost(TreatmentConfirmationFailure::EvidenceLost);
        }
        if observation.failure.is_some() {
            self.treatment_observation = Some(observation);
            if clean
                || self
                    .treatment_observation
                    .as_ref()
                    .is_some_and(|observation| observation.failure_without_drain)
            {
                self.finish_treatment();
            }
            return Ok(());
        }
        if !clean || observation.pending {
            self.treatment_observation = Some(observation);
            return Ok(());
        }
        // Existing source metadata and pause refreshes must reach the owner
        // validation path before this complete native treatment is published.
        // Do not add a treatment-specific metadata batch or assume invariance.
        if observation.index == 10
            && (observation.phase == Phase::Final
                || (observation.phase == Phase::Verify && !observation.vf_changed))
            && (self.metadata.is_some() || self.pause_pending)
        {
            self.treatment_observation = Some(observation);
            return Ok(());
        }
        if observation.index == 10 {
            match observation.phase {
                Phase::Snapshot => {
                    if observation.key.pass == TreatmentPass::Candidate {
                        self.treatment_restore
                            .as_mut()
                            .expect("reserved restore")
                            .snapshot_verified = true;
                    }
                    observation.phase = Phase::Write;
                    observation.index = 0;
                }
                Phase::Verify => {
                    if observation.vf_changed {
                        if let Some(retained) = &mut self.treatment_restore {
                            retained.vf_touched = true;
                        }
                        let compiled = observation.target.filters.clone();
                        crate::diagnostics::filter_begin(observation.key, &compiled.entries);
                        #[cfg(test)]
                        self.native_record(serde_json::json!({
                            "event":"admission", "key":observation.key, "request":observation.id,
                            "entries":compiled.entries,
                        }));
                        self.filter_observation = Some(FilterObservation::new(
                            observation.id,
                            observation.key,
                            compiled,
                            now,
                            self.log_sequence,
                            self.dropped_context,
                        ));
                        observation.phase = Phase::Filter;
                    } else {
                        self.treatment_observation = Some(observation);
                        self.finish_treatment();
                        return Ok(());
                    }
                }
                Phase::Final => {
                    self.treatment_observation = Some(observation);
                    self.finish_treatment();
                    return Ok(());
                }
                _ => unreachable!("only read sequences include pause"),
            }
        }
        if observation.phase == Phase::Write {
            while observation.index < 9 {
                if observation.different[observation.index] {
                    break;
                }
                observation.index += 1;
            }
            if observation.index == 9 {
                observation.phase = Phase::Verify;
                observation.index = 0;
            } else {
                observation.phase = Phase::Set;
            }
        }
        if observation.phase == Phase::Filter {
            self.treatment_observation = Some(observation);
            return Ok(());
        }
        let id = match observation.next_request() {
            Ok(id) => id,
            Err(_) => {
                observation.lost(TreatmentConfirmationFailure::BackendUnavailable);
                self.treatment_observation = Some(observation);
                self.finish_treatment();
                return Ok(());
            }
        };
        let operation = if observation.phase == Phase::Snapshot {
            RendererOperation::Snapshot
        } else {
            RendererOperation::Readback
        };
        let (pending, submitted) = if observation.index == 9 {
            (
                Pending::RendererPause {
                    key: observation.key,
                },
                self.get_renderer_async(observation.key, id, c"pause")?,
            )
        } else {
            let (property, value) = observation.target.renderer.native()[observation.index];
            if observation.phase == Phase::Set {
                observation.uncertain_set = true;
                (
                    Pending::RendererSet {
                        key: observation.key,
                        property,
                    },
                    self.set_renderer_async(observation.key, id, property, value)?,
                )
            } else {
                (
                    Pending::RendererRead {
                        key: observation.key,
                        property,
                        operation,
                    },
                    self.get_renderer_async(observation.key, id, native_name(property))?,
                )
            }
        };
        match submitted {
            Ok(()) => {
                observation.pending = true;
                self.pending.push((id, pending));
            }
            Err(mpv_error) => {
                observation.uncertain_set = false;
                if observation.index == 9 {
                    observation.lost(TreatmentConfirmationFailure::BackendUnavailable);
                } else {
                    let property = observation.target.renderer.native()[observation.index].0;
                    let operation = if observation.phase == Phase::Set {
                        RendererOperation::Set
                    } else {
                        operation
                    };
                    observation.renderer_failure(
                        property,
                        RendererErrorKind::Submission {
                            operation,
                            mpv_error,
                        },
                    );
                }
            }
        }
        self.treatment_observation = Some(observation);
        Ok(())
    }

    pub(super) fn renderer_reply(
        &mut self,
        id: u64,
        pending: Pending,
        kind: ReplyKind,
        error: i32,
        name: Option<&CStr>,
        value: Option<&NodeValue>,
    ) {
        #[cfg(test)]
        self.native_record(serde_json::json!({
            "event":"renderer_reply", "request":id, "kind":format!("{kind:?}"), "error":error,
            "property":name.and_then(|name| name.to_str().ok()),
            "value":format!("{value:?}")
        }));
        let Some(observation) = &mut self.treatment_observation else {
            return;
        };
        let key = match pending {
            Pending::RendererRead { key, .. }
            | Pending::RendererSet { key, .. }
            | Pending::RendererPause { key, .. } => key,
            _ => return,
        };
        if key != observation.key {
            return;
        }
        tracing::info!(key = ?key, request = id, phase = ?observation.phase, kind = ?kind, error, property = ?name, value = ?value, "renderer_owned_reply");
        observation.pending = false;
        match pending {
            Pending::RendererSet { property, .. } => {
                if kind != ReplyKind::SetProperty {
                    observation.lost(TreatmentConfirmationFailure::EvidenceLost);
                    return;
                }
                observation.uncertain_set = false;
                if error < 0 {
                    observation.renderer_failure(
                        property,
                        RendererErrorKind::Rejected { mpv_error: error },
                    );
                } else {
                    observation.mutated = true;
                    observation.phase = Phase::Readback;
                }
            }
            Pending::RendererRead {
                property,
                operation,
                ..
            } => {
                if kind != ReplyKind::Property || name != Some(native_name(property)) {
                    observation.renderer_failure(property, RendererErrorKind::MalformedReadback);
                    return;
                }
                if error < 0 {
                    observation.renderer_failure(
                        property,
                        RendererErrorKind::PropertyUnavailable {
                            operation,
                            mpv_error: error,
                        },
                    );
                    return;
                }
                let expected = if observation.phase == Phase::Snapshot
                    && key.pass == TreatmentPass::Candidate
                {
                    self.treatment_restore
                        .as_ref()
                        .expect("hot prior")
                        .prior
                        .renderer
                        .native()[observation.index]
                        .1
                } else {
                    observation.target.renderer.native()[observation.index].1
                };
                if !valid_native_type(property, expected, value) {
                    observation.renderer_failure(property, RendererErrorKind::MalformedReadback);
                    return;
                }
                if observation.phase == Phase::Snapshot {
                    observation.different[observation.index] = !matches_native(
                        property,
                        observation.target.renderer.native()[observation.index].1,
                        value,
                    );
                }
                // A restore snapshot observes the partially changed renderer. Its
                // retained verified pre-candidate snapshot remains the target.
                if !(observation.phase == Phase::Snapshot && key.pass == TreatmentPass::Restore)
                    && !matches_native(property, expected, value)
                {
                    let mismatch = if observation.phase == Phase::Snapshot {
                        RendererErrorKind::IncumbentMismatch
                    } else {
                        RendererErrorKind::ReadbackMismatch
                    };
                    observation.renderer_failure(property, mismatch);
                    return;
                }
                if observation.phase == Phase::Readback {
                    observation.phase = Phase::Write;
                }
                observation.index += 1;
            }
            Pending::RendererPause { .. } => {
                if kind != ReplyKind::Property
                    || name != Some(c"pause")
                    || error < 0
                    || !matches!(value, Some(NodeValue::Flag(paused)) if *paused == (observation.playback == InitialPlayback::Paused))
                {
                    observation.lost(TreatmentConfirmationFailure::PlaybackChanged);
                    return;
                }
                observation.index += 1;
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        capture::{CaptureMode, CapturedFourCc, FrameRate},
        filters::FilterChain,
        render::RendererSettings,
        state::{ApplyId, AttemptId},
    };

    fn key(apply: u64, pass: TreatmentPass) -> TreatmentAttemptKey {
        TreatmentAttemptKey {
            apply: ApplyId::new(apply).unwrap(),
            attempt: AttemptId::new(1).unwrap(),
            pass,
        }
    }
    fn reference() -> RendererSettings {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/FUR-016/reference-renderer.json"
        ))
        .unwrap()
    }
    fn node(value: NativeRendererValue) -> NodeValue {
        match value {
            NativeRendererValue::Choice(value) => NodeValue::String(value.to_owned()),
            NativeRendererValue::Flag(value) => NodeValue::Flag(value),
            NativeRendererValue::Int(value) => NodeValue::Int(value),
            NativeRendererValue::Float(value) => NodeValue::Double(value),
        }
    }
    fn incumbent(playback: InitialPlayback) -> (MpvBackend, [NodeValue; 9]) {
        use crate::capture::{input::CaptureSelection, linux};
        let mode = CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(320, 240).unwrap(),
            rate: FrameRate::new(30, 1).unwrap(),
        };
        let capture = linux::session_fixture(&["/dev/video0"], mode);
        let selection =
            CaptureSelection::from_snapshot(&capture, Path::new("/dev/video0"), mode).unwrap();
        let mut backend = MpvBackend::new(
            String::new(),
            selection.validate_snapshot(&capture).unwrap(),
            AudioSelection::default(),
            PlaybackGain::default(),
            playback,
            fixture_watch_stamp(),
            OutputPlan::Silent {
                revision: crate::domain::output::OutputRevision::first(),
                reason: crate::domain::output::OutputSilence::NoAvailableOutput,
            },
        );
        let treatments = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &RendererSettings::default(),
        );
        let values = std::array::from_fn(|index| node(treatments.renderer.native()[index].1));
        let confirmation = crate::media::controller::test_support::treatment_confirmation(
            key(1, TreatmentPass::Open),
            playback,
            RendererSettings::default(),
        );
        backend.filter_health = Some(FilterHealth {
            key: key(1, TreatmentPass::Open),
            treatment_key: key(1, TreatmentPass::Open),
            entries: Vec::new(),
            marker: 0,
            dropped_at_begin: 0,
            faulted: false,
        });
        backend.landed_treatments = Some((treatments, confirmation));
        backend.property_requests = Some(Vec::new());
        (backend, values)
    }

    fn answer(
        backend: &mut MpvBackend,
        current: &mut [NodeValue; 9],
        now: Instant,
        requests: &mut Vec<(&'static CStr, bool)>,
    ) {
        let Some((id, name, value)) = backend.property_requests.as_mut().unwrap().pop() else {
            return;
        };
        requests.push((name, value.is_some()));
        let index = backend
            .treatment_observation
            .as_ref()
            .unwrap()
            .target
            .renderer
            .native()
            .iter()
            .position(|(property, _)| native_name(*property) == name);
        if let Some(value) = value {
            current[index.unwrap()] = value;
            backend
                .reply_at(id, ReplyKind::SetProperty, 0, None, None, now)
                .unwrap();
        } else {
            let value = match name {
                value if value == c"pause" => NodeValue::Flag(backend.paused),
                _ => current[index.unwrap()].clone(),
            };
            let value = match (name, value) {
                (name, NodeValue::String(value))
                    if (name == c"cscale" || name == c"dscale") && value.as_c_str() == c"" =>
                {
                    NodeValue::Int(0)
                }
                (name, NodeValue::String(value))
                    if name == c"dither" && value.as_c_str() == c"no" =>
                {
                    NodeValue::Flag(false)
                }
                (_, value) => value,
            };
            backend
                .reply_at(
                    id,
                    ReplyKind::Property,
                    0,
                    Some(name.to_owned()),
                    Some(value),
                    now,
                )
                .unwrap();
        }
    }
    type SettledTreatment = (
        Result<TreatmentConfirmation, Box<TreatmentFailure>>,
        Vec<(&'static CStr, bool)>,
    );

    fn settle(backend: &mut MpvBackend, current: &mut [NodeValue; 9]) -> SettledTreatment {
        let now = backend.treatment_observation.as_ref().unwrap().admitted;
        let mut requests = Vec::new();
        for _ in 0..128 {
            backend.treatment_tick(now, true).unwrap();
            while let Some(event) = backend.emitted.pop_front() {
                match event {
                    // A complete renderer-only hot commit produces no capture or
                    // pause owner event, so those refreshes are not a
                    // prerequisite for the aggregate success. The mandatory
                    // renderer proof is the nine property reads plus the owned
                    // pause read driven by the treatment protocol, which each
                    // case still asserts through its recorded requests.
                    BackendEvent::Observed { .. } | BackendEvent::PauseObserved(_) => {}
                    BackendEvent::TreatmentResult { result, .. } => {
                        return (result, requests);
                    }
                    BackendEvent::CommandReply { error, .. } => assert_eq!(error, 0),
                    other => panic!("unexpected treatment event {other:?}"),
                }
            }
            answer(backend, current, now, &mut requests);
        }
        panic!("bounded fake renderer protocol did not settle");
    }

    #[test]
    fn renderer_only_live_and_paused_verify_every_property_without_reinstalling_vf() {
        for playback in [InitialPlayback::Live, InitialPlayback::Paused] {
            let (mut backend, mut current) = incumbent(playback);
            let target = crate::media::controller::test_support::prepared_treatments(
                &FilterChain::default(),
                &reference(),
            );
            backend
                .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
                .unwrap();
            let (result, requests) = settle(&mut backend, &mut current);
            let confirmation = result.unwrap();
            assert_eq!(confirmation.playback(), playback);
            assert_eq!(confirmation.key(), key(2, TreatmentPass::Candidate));
            assert_eq!(confirmation.renderer().settings(), &reference());
            assert_eq!(
                confirmation.filters().origin().key(),
                key(1, TreatmentPass::Open)
            );
            assert!(matches!(
                confirmation.filters(),
                FilterTreatmentProof::Preserved { .. }
            ));
            assert!(backend.filter_observation.is_none() && backend.filter_result.is_none());
            assert_eq!(backend.paused, playback == InitialPlayback::Paused);
            for (property, _) in backend
                .landed_treatments
                .as_ref()
                .unwrap()
                .0
                .renderer
                .native()
            {
                assert!(
                    requests
                        .iter()
                        .filter(|(name, set)| *name == native_name(*property) && !*set)
                        .count()
                        >= 2,
                    "snapshot and final read are mandatory even for unchanged/inactive {property:?}"
                );
            }
            let names: Vec<_> = requests
                .iter()
                .filter(|(_, set)| *set)
                .map(|(name, _)| *name)
                .collect();
            assert_eq!(
                names,
                vec![
                    c"scale",
                    c"cscale",
                    c"dscale",
                    c"scale-antiring",
                    c"dither",
                    c"deband-iterations",
                    c"deband-grain",
                    c"deband"
                ]
            );
        }
    }

    #[test]
    fn successful_set_reply_cannot_confirm_or_replace_separate_readback() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = backend.treatment_observation.as_ref().unwrap().admitted;
        let mut requests = Vec::new();
        for _ in 0..10 {
            backend.treatment_tick(now, true).unwrap();
            answer(&mut backend, &mut current, now, &mut requests);
        }
        backend.treatment_tick(now, true).unwrap();
        answer(&mut backend, &mut current, now, &mut requests);
        assert!(matches!(
            backend.treatment_observation.as_ref().unwrap().phase,
            Phase::Readback
        ));
        assert!(backend.emitted.is_empty());
        backend.treatment_tick(now, true).unwrap();
        let (id, name, value) = backend.property_requests.as_ref().unwrap().last().unwrap();
        assert_eq!(*name, c"scale");
        assert!(value.is_none());
        let id = *id;
        backend
            .reply_at(
                id + 1,
                ReplyKind::Property,
                0,
                Some(c"scale".to_owned()),
                Some(current[0].clone()),
                now,
            )
            .unwrap();
        assert!(backend.treatment_observation.as_ref().unwrap().pending);
        backend
            .reply_at(id, ReplyKind::SetProperty, 0, None, None, now)
            .unwrap();
        assert!(backend.treatment_observation.as_ref().unwrap().pending);
        assert!(backend.emitted.is_empty());
        let (result, _) = settle(&mut backend, &mut current);
        assert!(result.is_ok());
    }

    #[test]
    fn drift_and_malformed_snapshot_refuse_all_writes_with_truthful_dispositions() {
        for malformed in [false, true] {
            let (mut backend, mut current) = incumbent(InitialPlayback::Live);
            current[0] = if malformed {
                NodeValue::Int(7)
            } else {
                NodeValue::String(c"mitchell".to_owned())
            };
            let target = crate::media::controller::test_support::prepared_treatments(
                &FilterChain::default(),
                &reference(),
            );
            backend
                .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
                .unwrap();
            let (result, requests) = settle(&mut backend, &mut current);
            let failure = result.unwrap_err();
            assert!(!requests.iter().any(|(_, set)| *set));
            let TreatmentErrorKind::Renderer(renderer) = failure.kind else {
                panic!("structured renderer failure required");
            };
            assert_eq!(renderer.property, RendererProperty::Scale);
            assert_eq!(
                renderer.kind,
                if malformed {
                    RendererErrorKind::MalformedReadback
                } else {
                    RendererErrorKind::IncumbentMismatch
                }
            );
            assert_eq!(
                failure.disposition,
                if malformed {
                    TreatmentFailureDisposition::Untouched
                } else {
                    TreatmentFailureDisposition::RestoreFreshOwner
                }
            );
            assert_eq!(
                backend.landed_treatments.as_ref().unwrap().1.key(),
                key(1, TreatmentPass::Open)
            );
        }
    }

    #[test]
    fn settled_second_write_failure_restores_the_whole_prior_once_without_vf() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Paused);
        let prior = backend.landed_treatments.as_ref().unwrap().0.clone();
        let target = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = backend.treatment_observation.as_ref().unwrap().admitted;
        let mut requests = Vec::new();
        let mut rejected = false;
        for _ in 0..64 {
            backend.treatment_tick(now, true).unwrap();
            if backend
                .property_requests
                .as_ref()
                .unwrap()
                .last()
                .is_some_and(|(_, name, value)| *name == c"cscale" && value.is_some())
            {
                let (id, _, _) = backend.property_requests.as_mut().unwrap().pop().unwrap();
                backend
                    .reply_at(id, ReplyKind::SetProperty, -9, None, None, now)
                    .unwrap();
                rejected = true;
                break;
            }
            answer(&mut backend, &mut current, now, &mut requests);
            assert!(requests.len() < 32);
        }
        assert!(rejected, "bounded protocol reaches the second SET");
        let (result, _) = settle(&mut backend, &mut current);
        assert_eq!(
            result.unwrap_err().disposition,
            TreatmentFailureDisposition::RestoreSameOwner
        );
        assert_ne!(current[0], node(prior.renderer.native()[0].1));
        backend
            .begin_treatment(8, key(2, TreatmentPass::Restore), prior.clone())
            .unwrap();
        let (result, requests) = settle(&mut backend, &mut current);
        assert_eq!(
            result.unwrap().renderer().settings(),
            &RendererSettings::default()
        );
        assert_eq!(
            requests.iter().filter(|(_, set)| *set).count(),
            1,
            "only scale actually changed before rejection"
        );
        assert_eq!(
            current,
            std::array::from_fn(|index| node(prior.renderer.native()[index].1))
        );
        assert!(backend.filter_observation.is_none());
        assert!(
            backend
                .begin_treatment(9, key(2, TreatmentPass::Restore), prior)
                .is_err(),
            "retained route consumed, no second automatic restoration"
        );
    }

    #[test]
    fn unknown_set_completion_requires_fresh_owner_and_exhaustion_refuses_before_writing() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target.clone())
            .unwrap();
        let now = backend.treatment_observation.as_ref().unwrap().admitted;
        let mut requests = Vec::new();
        for _ in 0..10 {
            backend.treatment_tick(now, true).unwrap();
            answer(&mut backend, &mut current, now, &mut requests);
        }
        backend.treatment_tick(now, true).unwrap();
        backend
            .treatment_tick(now + FilterConfirmation::DEADLINE, true)
            .unwrap();
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = backend.emitted.pop_front()
        else {
            panic!("unknown completion failure");
        };
        assert_eq!(
            failure.disposition,
            TreatmentFailureDisposition::RestoreFreshOwner
        );
        assert!(matches!(
            failure.kind,
            TreatmentErrorKind::Renderer(RendererFailure {
                kind: RendererErrorKind::Deadline {
                    operation: RendererOperation::Set
                },
                ..
            })
        ));
        let (mut exhausted, _) = incumbent(InitialPlayback::Live);
        exhausted.last_internal = u64::MAX - 100;
        let target = crate::media::controller::test_support::prepared_treatments(
            &changed_graph(),
            &reference(),
        );
        exhausted
            .submit(
                RequestId::for_test(9),
                BackendCommand::ApplyTreatments {
                    key: key(2, TreatmentPass::Candidate),
                    treatments: Box::new(target),
                },
            )
            .unwrap();
        assert!(exhausted.property_requests.as_ref().unwrap().is_empty());
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = exhausted.emitted.pop_front()
        else {
            panic!("pre-mutation exhaustion refusal");
        };
        assert_eq!(failure.disposition, TreatmentFailureDisposition::Untouched);
    }

    #[test]
    fn renderer_reconfiguration_cannot_satisfy_the_later_vf_window_or_commit_partially() {
        use crate::domain::filters::{
            ColorLevels, Filter, FilterEntry, FormatParams, SdrGamma, SdrMatrix,
        };
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let prior = backend.landed_treatments.as_ref().unwrap().0.clone();
        let chain = FilterChain::new(vec![FilterEntry::new(
            "indivisible vf and renderer".into(),
            Filter::Format(FormatParams::new(
                SdrMatrix::Bt709,
                ColorLevels::Limited,
                SdrGamma::Bt1886,
            )),
            true,
        )])
        .unwrap();
        let target =
            crate::media::controller::test_support::prepared_treatments(&chain, &reference());
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let admitted = backend.treatment_observation.as_ref().unwrap().admitted;
        let now = admitted + std::time::Duration::from_secs(5);
        backend.filter_reconfigured();
        let mut requests = Vec::new();
        for _ in 0..128 {
            backend.treatment_tick(now, true).unwrap();
            if backend.filter_observation.is_some() {
                break;
            }
            answer(&mut backend, &mut current, now, &mut requests);
        }
        let filter = backend
            .filter_observation
            .as_mut()
            .expect("vf admitted only after complete renderer verification");
        assert_eq!(filter.admitted, now);
        assert!(!filter.reconfigured);
        assert_eq!(filter.advances, 0);
        filter.outcome = Some(0); // Fake command succeeded but no reconfiguration witness.
        filter.failure = Some(FilterConfirmationFailure::MissingReconfig);
        backend.finish_filter();
        backend.treatment_tick(now, true).unwrap();
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = backend.emitted.pop_front()
        else {
            panic!("no partial renderer success");
        };
        assert_eq!(
            failure.disposition,
            TreatmentFailureDisposition::RestoreSameOwner
        );
        assert_eq!(backend.landed_treatments.as_ref().unwrap().0, prior);
        assert_eq!(
            backend.landed_treatments.as_ref().unwrap().1.key(),
            key(1, TreatmentPass::Open)
        );
        assert!(
            requests.iter().any(|(_, set)| *set),
            "renderer actually changed in the fake native state"
        );
    }

    #[test]
    fn pinned_choice_node_aliases_are_property_specific_and_keep_string_forms() {
        for property in [RendererProperty::Cscale, RendererProperty::Dscale] {
            let inherit = NativeRendererValue::Choice(c"");
            assert!(valid_native_type(
                property,
                inherit,
                Some(&NodeValue::Int(0))
            ));
            assert!(matches_native(property, inherit, Some(&NodeValue::Int(0))));
            assert!(matches_native(
                property,
                inherit,
                Some(&NodeValue::String(c"".to_owned()))
            ));
            assert!(!matches_native(
                property,
                NativeRendererValue::Choice(c"lanczos"),
                Some(&NodeValue::Int(0))
            ));
            for value in [
                NodeValue::Int(1),
                NodeValue::Int(-1),
                NodeValue::Flag(false),
            ] {
                assert!(!valid_native_type(property, inherit, Some(&value)));
            }
        }
        let no = NativeRendererValue::Choice(c"no");
        assert!(valid_native_type(
            RendererProperty::Dither,
            no,
            Some(&NodeValue::Flag(false))
        ));
        assert!(matches_native(
            RendererProperty::Dither,
            no,
            Some(&NodeValue::Flag(false))
        ));
        assert!(matches_native(
            RendererProperty::Dither,
            no,
            Some(&NodeValue::String(c"no".to_owned()))
        ));
        assert!(!matches_native(
            RendererProperty::Dither,
            NativeRendererValue::Choice(c"fruit"),
            Some(&NodeValue::Flag(false))
        ));
        for value in [NodeValue::Int(0), NodeValue::Flag(true)] {
            assert!(!valid_native_type(
                RendererProperty::Dither,
                no,
                Some(&value)
            ));
        }
        assert!(!valid_native_type(
            RendererProperty::Scale,
            NativeRendererValue::Choice(c"lanczos"),
            Some(&NodeValue::Int(0))
        ));
        assert!(!valid_native_type(
            RendererProperty::Scale,
            NativeRendererValue::Choice(c"lanczos"),
            Some(&NodeValue::Flag(false))
        ));
    }

    #[test]
    fn native_inherit_and_dither_no_readbacks_confirm_without_changing_set_formats() {
        use crate::domain::render::{DitherMode, ScalerChoice};
        let (mut backend, mut current) = incumbent(InitialPlayback::Paused);
        let mut values = RendererSettings::default().values();
        values.dscale = ScalerChoice::Inherit;
        values.dither = DitherMode::No;
        let settings = RendererSettings::new(values).unwrap();
        let target = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &settings,
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let (result, requests) = settle(&mut backend, &mut current);
        assert_eq!(result.unwrap().renderer().settings(), &settings);
        assert_eq!(
            current[2],
            NodeValue::String(c"".to_owned()),
            "inherit SET remains STRING"
        );
        assert_eq!(
            current[4],
            NodeValue::String(c"no".to_owned()),
            "dither=no SET remains STRING"
        );
        assert_eq!(requests.iter().filter(|(_, set)| *set).count(), 2);
        for (property, _) in backend
            .landed_treatments
            .as_ref()
            .unwrap()
            .0
            .renderer
            .native()
        {
            assert!(
                requests
                    .iter()
                    .filter(|(name, set)| *name == native_name(*property) && !*set)
                    .count()
                    >= 2
            );
        }
        assert!(backend.filter_observation.is_none());
    }

    #[test]
    fn canonical_binary32_equality_is_exact_and_cancellation_wins_between_requests() {
        let compiled = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &reference(),
        );
        let expected = compiled.renderer.native()[3].1;
        assert!(matches_native(
            RendererProperty::ScaleAntiring,
            expected,
            Some(&NodeValue::Double(f64::from(0.7_f32)))
        ));
        assert!(!matches_native(
            RendererProperty::ScaleAntiring,
            expected,
            Some(&NodeValue::Double(0.7))
        ));
        assert!(!matches_native(
            RendererProperty::ScaleAntiring,
            expected,
            Some(&NodeValue::Double(f64::NAN))
        ));
        assert_eq!(compiled.renderer.requested().values().scale_antiring, 0.7);
        let (mut backend, _) = incumbent(InitialPlayback::Live);
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), compiled)
            .unwrap();
        let now = backend.treatment_observation.as_ref().unwrap().admitted;
        backend.treatment_tick(now, true).unwrap();
        let issued = backend.property_requests.as_ref().unwrap().len();
        backend.treatment_lost(TreatmentConfirmationFailure::EvidenceLost);
        backend.treatment_tick(now, true).unwrap();
        assert_eq!(
            backend.property_requests.as_ref().unwrap().len(),
            issued,
            "no next request after evidence loss"
        );
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = backend.emitted.pop_front()
        else {
            panic!("evidence loss must dominate");
        };
        assert_eq!(
            failure.disposition,
            TreatmentFailureDisposition::RestoreFreshOwner
        );
        assert!(backend.pending.is_empty());
    }

    #[test]
    fn known_vf_poison_refuses_with_typed_frozen_diagnostics_before_native_work() {
        for filters in [FilterChain::default(), changed_graph()] {
            let (mut backend, _) = incumbent(InitialPlayback::Live);
            let prior = backend.landed_treatments.clone();
            backend.filters_poisoned = true;
            let target =
                crate::media::controller::test_support::prepared_treatments(&filters, &reference());
            let entries = target.filters.entries.clone();
            let requested = key(2, TreatmentPass::Candidate);
            backend
                .submit(
                    RequestId::for_test(7),
                    BackendCommand::ApplyTreatments {
                        key: requested,
                        treatments: Box::new(target),
                    },
                )
                .unwrap();
            let now = backend.treatment_observation.as_ref().unwrap().admitted;
            backend.treatment_tick(now, false).unwrap();
            let Some(BackendEvent::TreatmentResult {
                key: observed,
                result: Err(failure),
                ..
            }) = backend.emitted.pop_front()
            else {
                panic!("known poisoned vf must return a typed refusal");
            };
            assert_eq!(observed, requested);
            assert_eq!(failure.key, Some(requested));
            assert_eq!(
                failure.disposition,
                TreatmentFailureDisposition::RestoreFreshOwner
            );
            let TreatmentErrorKind::Filter(filter) = failure.kind else {
                panic!("known vf poison must retain the vf failure type");
            };
            assert_eq!(
                filter.kind,
                FilterErrorKind::Unconfirmed {
                    reason: FilterConfirmationFailure::BackendUnavailable,
                }
            );
            assert!(filter.requires_fresh_owner);
            assert_eq!(filter.attributed_ordinal, None);
            assert_eq!(filter.diagnostics.key, Some(requested));
            assert_eq!(filter.diagnostics.entries, entries);
            assert!(backend.property_requests.as_ref().unwrap().is_empty());
            assert!(backend.pending.is_empty());
            assert!(backend.filter_observation.is_none());
            assert!(backend.filter_result.is_none());
            assert_eq!(backend.landed_treatments, prior);
        }
    }

    #[test]
    fn first_set_definitively_rejected_keeps_the_verified_prior_untouched() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let prior = current.clone();
        let target = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = backend.treatment_observation.as_ref().unwrap().admitted;
        let mut requests = Vec::new();
        for _ in 0..10 {
            backend.treatment_tick(now, true).unwrap();
            answer(&mut backend, &mut current, now, &mut requests);
        }
        backend.treatment_tick(now, true).unwrap();
        let (id, property, value) = backend.property_requests.as_mut().unwrap().pop().unwrap();
        assert_eq!(property, c"scale");
        assert!(value.is_some());
        backend
            .reply_at(id, ReplyKind::SetProperty, -9, None, None, now)
            .unwrap();
        let (result, later_requests) = settle(&mut backend, &mut current);
        let failure = result.unwrap_err();
        assert_eq!(failure.disposition, TreatmentFailureDisposition::Untouched);
        assert!(later_requests.is_empty());
        assert_eq!(current, prior);
        assert_eq!(
            backend.landed_treatments.as_ref().unwrap().1.key(),
            key(1, TreatmentPass::Open)
        );
        assert!(backend.filter_observation.is_none());
    }

    fn changed_graph() -> FilterChain {
        use crate::domain::filters::{
            ColorLevels, Filter, FilterEntry, FormatParams, SdrGamma, SdrMatrix,
        };
        FilterChain::new(vec![FilterEntry::new(
            "real producer-path unit graph".into(),
            Filter::Format(FormatParams::new(
                SdrMatrix::Bt709,
                ColorLevels::Limited,
                SdrGamma::Bt1886,
            )),
            true,
        )])
        .unwrap()
    }

    fn advance_to(
        backend: &mut MpvBackend,
        current: &mut [NodeValue; 9],
        phase: Phase,
        index: usize,
    ) -> Instant {
        let now = backend.treatment_observation.as_ref().unwrap().admitted;
        let mut requests = Vec::new();
        for _ in 0..128 {
            let observation = backend
                .treatment_observation
                .as_ref()
                .expect("not prematurely settled");
            if observation.phase == phase && observation.index == index {
                return now;
            }
            backend.treatment_tick(now, true).unwrap();
            answer(backend, current, now, &mut requests);
        }
        panic!("bounded protocol did not reach {phase:?}/{index}");
    }

    #[test]
    fn pending_source_refresh_blocks_complete_renderer_proof_and_expires_safely() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let prior = backend.landed_treatments.clone();
        let target = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = advance_to(&mut backend, &mut current, Phase::Verify, 10);
        backend.refresh_observed(false).unwrap();
        let issued = backend.property_requests.as_ref().unwrap().len();
        backend.treatment_tick(now, true).unwrap();
        assert!(backend.treatment_observation.is_some());
        assert!(backend.emitted.is_empty());
        assert_eq!(backend.landed_treatments, prior);
        backend
            .treatment_tick(now + FilterConfirmation::DEADLINE, true)
            .unwrap();
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = backend.emitted.pop_front()
        else {
            panic!("unsettled source facts cannot authorize a complete proof");
        };
        assert_eq!(
            failure.disposition,
            TreatmentFailureDisposition::RestoreFreshOwner
        );
        assert_eq!(backend.property_requests.as_ref().unwrap().len(), issued);
        assert_eq!(backend.landed_treatments, prior);
    }

    #[test]
    fn native_graph_revocation_during_renderer_verification_cannot_publish_preserved_success() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let chain = changed_graph();
        let prior = crate::media::controller::test_support::prepared_treatments(
            &chain,
            &RendererSettings::default(),
        );
        backend.filter_health.as_mut().unwrap().entries = prior.filters.entries.clone();
        backend.landed_treatments.as_mut().unwrap().0 = prior;
        let target =
            crate::media::controller::test_support::prepared_treatments(&chain, &reference());
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = advance_to(&mut backend, &mut current, Phase::Verify, 4);
        backend
            .filter_log(
                "vf".into(),
                "error".into(),
                "Disabling filter furami_0 because it has failed.".into(),
            )
            .unwrap();
        backend.treatment_tick(now, false).unwrap();
        assert!(backend.treatment_observation.is_none());
        assert!(backend.pending.is_empty());
        let events: Vec<_> = backend.emitted.drain(..).collect();
        assert!(events.iter().any(|event| matches!(event,
            BackendEvent::TreatmentFault { key: applied, .. } if *applied == key(1, TreatmentPass::Open))));
        assert!(events.iter().any(|event| matches!(event,
            BackendEvent::TreatmentResult { key: attempted, result: Err(failure), .. }
                if *attempted == key(2, TreatmentPass::Candidate)
                    && failure.disposition == TreatmentFailureDisposition::RestoreFreshOwner
                    && matches!(&failure.kind, TreatmentErrorKind::Filter(filter)
                        if filter.kind == FilterErrorKind::RuntimeGraph))));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::TreatmentResult { result: Ok(_), .. }))
        );
        assert_eq!(
            backend.landed_treatments.as_ref().unwrap().1.key(),
            key(1, TreatmentPass::Open)
        );
    }

    #[test]
    fn graph_revocation_after_vf_confirmation_keeps_origin_but_revokes_last_applied_identity() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(
            &changed_graph(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = advance_to(&mut backend, &mut current, Phase::Filter, 10);
        let filter = backend.filter_observation.as_mut().unwrap();
        filter.submitted = true;
        filter.outcome = Some(0);
        filter.reconfigured = true;
        filter.baseline = Some(0.0);
        filter.position = Some(2.0);
        filter.advances = 32;
        backend.finish_filter();
        assert_eq!(
            backend.filter_health.as_ref().unwrap().key,
            key(2, TreatmentPass::Candidate)
        );
        assert_eq!(
            backend.filter_health.as_ref().unwrap().treatment_key,
            key(1, TreatmentPass::Open)
        );
        backend.treatment_tick(now, true).unwrap();
        assert_eq!(
            backend.treatment_observation.as_ref().unwrap().phase,
            Phase::Final
        );
        backend
            .filter_log(
                "vf".into(),
                "error".into(),
                "Disabling filter furami_0 because it has failed.".into(),
            )
            .unwrap();
        backend.treatment_tick(now, false).unwrap();
        let events: Vec<_> = backend.emitted.drain(..).collect();
        assert!(events.iter().any(|event| matches!(event,
            BackendEvent::TreatmentFault { key: applied, failure }
                if *applied == key(1, TreatmentPass::Open)
                    && matches!(&failure.kind, TreatmentErrorKind::Filter(filter)
                        if filter.diagnostics.key == Some(key(2, TreatmentPass::Candidate))))));
        assert!(events.iter().any(|event| matches!(event,
            BackendEvent::TreatmentResult { result: Err(failure), .. }
                if failure.disposition == TreatmentFailureDisposition::RestoreFreshOwner)));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, BackendEvent::TreatmentResult { result: Ok(_), .. }))
        );
        assert_eq!(
            backend.landed_treatments.as_ref().unwrap().1.key(),
            key(1, TreatmentPass::Open)
        );
    }

    #[test]
    fn sustained_nonclean_events_settle_renderer_and_vf_deadlines_without_faking_none() {
        let (mut backend, _) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = backend.treatment_observation.as_ref().unwrap().admitted;
        backend.treatment_tick(now, true).unwrap(); // one actual fake-native GET pending.
        for step in 1..100 {
            backend
                .treatment_tick(now + std::time::Duration::from_millis(step * 100), false)
                .unwrap();
            assert!(backend.emitted.is_empty());
        }
        backend
            .treatment_tick(now + FilterConfirmation::DEADLINE, false)
            .unwrap();
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = backend.emitted.pop_front()
        else {
            panic!("the failure must settle at the deadline without NONE");
        };
        assert_eq!(
            failure.disposition,
            TreatmentFailureDisposition::RestoreFreshOwner
        );
        assert!(matches!(
            failure.kind,
            TreatmentErrorKind::Renderer(RendererFailure {
                kind: RendererErrorKind::Deadline {
                    operation: RendererOperation::Snapshot
                },
                ..
            })
        ));
        assert!(backend.treatment_observation.is_none());

        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(
            &changed_graph(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        advance_to(&mut backend, &mut current, Phase::Filter, 10);
        let now =
            backend.filter_observation.as_ref().unwrap().admitted + FilterConfirmation::DEADLINE;
        backend.filter_tick(now, false).unwrap();
        backend.treatment_tick(now, false).unwrap();
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = backend.emitted.pop_front()
        else {
            panic!("the genuine vf deadline negative must not wait for NONE");
        };
        assert_eq!(
            failure.disposition,
            TreatmentFailureDisposition::RestoreFreshOwner
        );
        assert!(matches!(
            failure.kind,
            TreatmentErrorKind::Filter(FilterFailure {
                kind: FilterErrorKind::Unconfirmed {
                    reason: FilterConfirmationFailure::Deadline
                },
                ..
            })
        ));
        assert!(backend.treatment_observation.is_none());
    }

    #[test]
    fn malformed_native_log_producer_ticks_and_settles_aggregate_without_none() {
        let (mut backend, _) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = backend.treatment_observation.as_ref().unwrap().admitted;
        let first = backend.malformed_filter_log(now).unwrap();
        assert!(!matches!(
            first,
            BackendEvent::None | BackendEvent::TreatmentResult { result: Ok(_), .. }
        ));
        assert!(backend.treatment_observation.is_none());
        assert!(backend.emitted.iter().any(|event| matches!(event,
            BackendEvent::TreatmentResult { result: Err(failure), .. }
                if failure.disposition == TreatmentFailureDisposition::RestoreFreshOwner)));
        for _ in 0..64 {
            assert!(!matches!(
                backend.malformed_filter_log(now).unwrap(),
                BackendEvent::None | BackendEvent::TreatmentResult { result: Ok(_), .. }
            ));
        }
    }

    #[test]
    fn raw_flag_two_is_rejected_by_actual_property_reply_producer_for_renderer_and_pause() {
        for property in [
            Some(RendererProperty::Deband),
            Some(RendererProperty::Interpolation),
            Some(RendererProperty::Dither),
            None,
        ] {
            let (mut backend, _) = incumbent(InitialPlayback::Paused);
            let mut values = reference().values();
            values.interpolation = true;
            let settings = RendererSettings::new(values).unwrap();
            let prior = crate::media::controller::test_support::prepared_treatments(
                &FilterChain::default(),
                &settings,
            );
            let mut current = std::array::from_fn(|index| node(prior.renderer.native()[index].1));
            let index = property.map_or(9, |property| {
                prior
                    .renderer
                    .native()
                    .iter()
                    .position(|(candidate, _)| *candidate == property)
                    .unwrap()
            });
            backend.landed_treatments = Some((
                prior.clone(),
                crate::media::controller::test_support::treatment_confirmation(
                    key(1, TreatmentPass::Open),
                    InitialPlayback::Paused,
                    settings,
                ),
            ));
            backend
                .begin_treatment(7, key(2, TreatmentPass::Candidate), prior)
                .unwrap();
            let now = advance_to(&mut backend, &mut current, Phase::Snapshot, index);
            backend.treatment_tick(now, true).unwrap();
            let (id, name, submitted) = backend.property_requests.as_mut().unwrap().pop().unwrap();
            assert!(submitted.is_none());
            let mut raw = MpvNode {
                value: MpvNodeValue { flag: 2 },
                format: 3,
            };
            let mut payload = MpvPropertyEvent {
                name: name.as_ptr(),
                format: 6,
                data: ptr::from_mut(&mut raw).cast(),
            };
            // SAFETY: Every native-layout payload/name pointer refers to a live
            // stack/static allocation until the real producer finishes copying.
            let result =
                unsafe { backend.property_reply_event(id, 0, ptr::from_mut(&mut payload).cast()) }
                    .unwrap();
            assert!(matches!(result, BackendEvent::Other));
            backend.treatment_tick(now, false).unwrap();
            let Some(BackendEvent::TreatmentResult {
                result: Err(failure),
                ..
            }) = backend.emitted.pop_front()
            else {
                panic!("raw invalid FLAG must produce an aggregate failure, never true");
            };
            assert_eq!(
                failure.disposition,
                TreatmentFailureDisposition::RestoreFreshOwner
            );
            match (property, failure.kind) {
                (Some(expected), TreatmentErrorKind::Renderer(renderer)) => {
                    assert_eq!(renderer.property, expected);
                    assert_eq!(renderer.kind, RendererErrorKind::MalformedReadback);
                }
                (None, TreatmentErrorKind::Unconfirmed { reason }) => {
                    assert_eq!(reason, TreatmentConfirmationFailure::PlaybackChanged);
                }
                (_, other) => panic!("wrong property-aware malformed payload failure: {other:?}"),
            }
            assert!(backend.pending.is_empty());
            assert_eq!(
                backend.landed_treatments.as_ref().unwrap().1.key(),
                key(1, TreatmentPass::Open)
            );
        }
    }

    #[test]
    fn insufficient_reservation_is_atomic_and_untouched_before_any_property_write() {
        let (mut backend, current) = incumbent(InitialPlayback::Live);
        let before = backend.landed_treatments.clone();
        backend.last_internal = u64::MAX - (PASS_REQUESTS * 2 - 1);
        let target = crate::media::controller::test_support::prepared_treatments(
            &changed_graph(),
            &reference(),
        );
        backend
            .submit(
                RequestId::for_test(7),
                BackendCommand::ApplyTreatments {
                    key: key(2, TreatmentPass::Candidate),
                    treatments: Box::new(target),
                },
            )
            .unwrap();
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = backend.emitted.pop_front()
        else {
            panic!("atomic admission refusal must be reported");
        };
        assert_eq!(failure.disposition, TreatmentFailureDisposition::Untouched);
        assert_eq!(backend.last_internal, u64::MAX - (PASS_REQUESTS * 2 - 1));
        assert!(backend.property_requests.as_ref().unwrap().is_empty());
        assert!(backend.pending.is_empty());
        assert!(backend.treatment_observation.is_none());
        assert!(backend.treatment_restore.is_none());
        assert_eq!(backend.landed_treatments, before);
        assert_eq!(
            current,
            std::array::from_fn(|index| node(
                before.as_ref().unwrap().0.renderer.native()[index].1
            ))
        );
    }

    #[test]
    fn same_owner_restore_uses_reserved_requests_at_the_exact_boundary() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Paused);
        let prior = backend.landed_treatments.as_ref().unwrap().0.clone();
        backend.last_internal = u64::MAX - PASS_REQUESTS * 2;
        let target = crate::media::controller::test_support::prepared_treatments(
            &FilterChain::default(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = advance_to(&mut backend, &mut current, Phase::Write, 1);
        backend.treatment_tick(now, true).unwrap();
        let (id, _, _) = backend.property_requests.as_mut().unwrap().pop().unwrap();
        backend
            .reply_at(id, ReplyKind::SetProperty, -4, None, None, now)
            .unwrap();
        backend.treatment_tick(now, true).unwrap();
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = backend.emitted.pop_front()
        else {
            panic!("settled second write must fail");
        };
        assert_eq!(
            failure.disposition,
            TreatmentFailureDisposition::RestoreSameOwner
        );
        assert_eq!(backend.last_internal, u64::MAX);
        backend
            .begin_treatment(8, key(2, TreatmentPass::Restore), prior.clone())
            .unwrap();
        let (result, requests) = settle(&mut backend, &mut current);
        assert_eq!(result.unwrap().key(), key(2, TreatmentPass::Restore));
        assert_eq!(backend.landed_treatments.as_ref().unwrap().0, prior);
        assert_eq!(backend.last_internal, u64::MAX);
        for (property, _) in prior.renderer.native() {
            assert!(
                requests
                    .iter()
                    .filter(|(name, set)| *name == native_name(*property) && !*set)
                    .count()
                    >= 2
            );
        }
        assert!(backend.pending.is_empty());
    }

    #[test]
    fn combined_changed_vf_requires_reconfig_and_32_timed_samples_before_final_readback() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        backend.last_internal = u64::MAX - PASS_REQUESTS * 2;
        let target = crate::media::controller::test_support::prepared_treatments(
            &changed_graph(),
            &reference(),
        );
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        assert_eq!(backend.reconfiguration_event(), BackendEvent::VideoReconfig);
        let now = advance_to(&mut backend, &mut current, Phase::Filter, 10);
        let filter = backend.filter_observation.as_mut().unwrap();
        assert!(
            !filter.reconfigured,
            "renderer reconfiguration predates vf admission"
        );
        // Existing unit seam immediately after the native argv submission.
        // The command reply, reconfig producer, timed GETs and proof are real code.
        filter.submitted = true;
        filter.command.take();
        let command = filter.id;
        let admitted = filter.admitted;
        assert_eq!(admitted, now);
        backend
            .reply_at(command, ReplyKind::Command, 0, None, None, admitted)
            .unwrap();
        assert_eq!(backend.reconfiguration_event(), BackendEvent::VideoReconfig);
        let mut clock = admitted;
        for position in 0..=32 {
            if position != 0 {
                clock += FilterConfirmation::SAMPLE_INTERVAL;
            }
            backend.filter_tick(clock, false).unwrap();
            let (id, name, value) = backend.property_requests.as_mut().unwrap().pop().unwrap();
            assert_eq!(name, c"time-pos");
            assert!(value.is_none());
            backend
                .reply_at(
                    id,
                    ReplyKind::Property,
                    0,
                    Some(name.to_owned()),
                    Some(NodeValue::Double(f64::from(position))),
                    clock,
                )
                .unwrap();
            assert!(
                !backend.emitted.iter().any(|event| matches!(
                    event,
                    BackendEvent::TreatmentResult { result: Ok(_), .. }
                ))
            );
        }
        backend.filter_tick(clock, true).unwrap();
        backend.treatment_tick(clock, true).unwrap();
        assert_eq!(
            backend.treatment_observation.as_ref().unwrap().phase,
            Phase::Final
        );
        let (result, requests) = settle(&mut backend, &mut current);
        let confirmation = result.unwrap();
        assert!(matches!(
            confirmation.filters(),
            FilterTreatmentProof::Reconfigured(_)
        ));
        assert_eq!(confirmation.renderer().settings(), &reference());
        assert_eq!(backend.last_internal, u64::MAX);
        for (property, _) in backend
            .landed_treatments
            .as_ref()
            .unwrap()
            .0
            .renderer
            .native()
        {
            assert!(
                requests
                    .iter()
                    .any(|(name, set)| *name == native_name(*property) && !*set)
            );
        }
        assert!(backend.pending.is_empty());
    }
}
