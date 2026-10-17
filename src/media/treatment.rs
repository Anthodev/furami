//! Owner-only renderer transaction. Native replies, never requested values,
//! authorize the complete proof after an actual clean native drain.
use super::*;
use crate::domain::{
    failure::{
        RendererErrorKind, RendererFailure, RendererOperation, TreatmentConfirmationFailure,
        TreatmentErrorKind, TreatmentFailure, TreatmentFailureDisposition,
    },
    render::RendererProperty,
    state::{RendererConfirmation, TreatmentConfirmation, TreatmentPass},
};
use crate::media::{
    controller::PreparedTreatments,
    render_catalog::{NativeRendererValue, native_name},
};

pub(super) fn install_renderer_settings(
    handle: &Handle,
    renderer: &crate::media::render_catalog::CompiledRendererSettings,
) -> Result<(), MediaError> {
    for &(property, value) in renderer.native() {
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Snapshot,
    Write,
    Set,
    Readback,
    Verify,
}

fn phase_token(phase: Phase) -> &'static str {
    match phase {
        Phase::Snapshot => "Snapshot",
        Phase::Write => "Write",
        Phase::Set => "Set",
        Phase::Readback => "Readback",
        Phase::Verify => "Verify",
    }
}

// Snapshot (10), SET/readback (18), and full verification (10).
// Reserve candidate and its sole restoration before the first native write.
const PASS_REQUESTS: u64 = 38;
const TREATMENT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

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
    admitted: Instant,
    request: u64,
    request_end: u64,
    failure: Option<TreatmentFailure>,
    failure_without_drain: bool,
}

pub(super) struct TreatmentRestore {
    apply: crate::domain::state::ApplyId,
    prior: PreparedTreatments,
    playback: InitialPlayback,
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
        let proof_lost = self.failure.as_ref().is_some_and(|failure| {
            failure.disposition == TreatmentFailureDisposition::RestoreFreshOwner
        });
        // A correlated definitive SET refusal remains the primary cause even
        // when native evidence loss arrived first; it cannot restore health.
        if matches!(
            kind,
            RendererErrorKind::Rejected { .. }
                | RendererErrorKind::Submission {
                    operation: RendererOperation::Set,
                    ..
                }
        ) && self
            .failure
            .as_ref()
            .is_some_and(|failure| matches!(failure.kind, TreatmentErrorKind::Unconfirmed { .. }))
        {
            self.failure = None;
        }
        let disposition = if proof_lost {
            TreatmentFailureDisposition::RestoreFreshOwner
        } else {
            match kind {
                RendererErrorKind::IncumbentMismatch | RendererErrorKind::MalformedReadback => {
                    TreatmentFailureDisposition::RestoreFreshOwner
                }
                RendererErrorKind::ReadbackMismatch => {
                    TreatmentFailureDisposition::RestoreSameOwner
                }
                _ => self.disposition(),
            }
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
        tracing::info!(
            event_code = "renderer_get_submit",
            apply_id = key.apply.get(),
            attempt_id = key.attempt.get(),
            pass = super::pass_token(key.pass),
            request_id = id,
            property = super::renderer_property_token(name.to_str().unwrap_or("Unknown")),
        );
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
        tracing::info!(
            event_code = "renderer_set_submit",
            apply_id = key.apply.get(),
            attempt_id = key.attempt.get(),
            pass = super::pass_token(key.pass),
            request_id = id,
            property =
                super::renderer_property_token(native_name(property).to_str().unwrap_or("Unknown")),
        );
        #[cfg(test)]
        {
            self.native_record(serde_json::json!({"event":"renderer_set","key":key,"request":id,"property":native_name(property).to_str().expect("static name"),"value":format!("{value:?}")}));
            if let Some(error) = self.renderer_submission_error.take() {
                return Ok(Err(error));
            }
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
        let renderer = match &self.input {
            BackendInput::QualificationVideo { renderer, .. } => renderer,
            BackendInput::Capture(_) => {
                &self
                    .initial_treatments
                    .as_ref()
                    .ok_or_else(|| {
                        MediaError::new("treatment_options", "missing prepared initial treatments")
                    })?
                    .1
                    .renderer
            }
        };
        install_renderer_settings(handle, renderer)
    }

    pub(super) fn begin_treatment(
        &mut self,
        id: u64,
        key: TreatmentAttemptKey,
        target: PreparedTreatments,
    ) -> Result<(), MediaError> {
        if self.treatment_observation.is_some() {
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
        let (request, request_end) = if key.pass == TreatmentPass::Restore {
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
            (retained.request, retained.request_end)
        } else {
            let prior = self.landed_treatments.as_ref();
            if key.pass != TreatmentPass::Open && prior.is_none() {
                return Err(MediaError::new(
                    "treatment_admission",
                    "incumbent treatment proof unavailable",
                ));
            }
            // Reserve candidate and its sole restoration before any native write.
            // Incidental source metadata retains its existing request path.
            let request = self.last_internal;
            let end = request.checked_add(PASS_REQUESTS * 2).ok_or_else(|| {
                MediaError::new(
                    "request_exhausted",
                    "cannot reserve treatment and restore requests",
                )
            })?;
            self.last_internal = end;
            if let Some((prior, _)) = prior {
                self.treatment_restore = Some(TreatmentRestore {
                    apply: key.apply,
                    prior: prior.clone(),
                    playback,
                    snapshot_verified: false,
                    restore_admitted: false,
                    request: request + PASS_REQUESTS,
                    request_end: end,
                });
            }
            (request, request + PASS_REQUESTS)
        };
        let failure = self.treatment_poisoned.then_some(TreatmentFailure {
            key: Some(key),
            kind: TreatmentErrorKind::Unconfirmed {
                reason: TreatmentConfirmationFailure::EvidenceLost,
            },
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
            unattributed_error: false,
            admitted: Instant::now(),
            request,
            request_end,
            failure,
            failure_without_drain: self.treatment_poisoned,
        };
        tracing::info!(
            event_code = "treatment_admitted",
            apply_id = key.apply.get(),
            attempt_id = key.attempt.get(),
            pass = super::pass_token(key.pass),
            request_id = id,
        );
        #[cfg(test)]
        self.native_record(serde_json::json!({
            "event":"treatment_admission", "key":key, "request":id,
            "playback":playback,
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

    pub(super) fn finish_treatment(&mut self) {
        let mut observation = self
            .treatment_observation
            .take()
            .expect("settled treatment");
        if self.treatment_poisoned {
            observation.lost(TreatmentConfirmationFailure::EvidenceLost);
        }
        self.pending.retain(|(_, pending)| !matches!(pending,
            Pending::RendererRead { key, .. } | Pending::RendererSet { key, .. } | Pending::RendererPause { key, .. }
            if *key == observation.key));
        if let Some(failure) = &observation.failure
            && failure.disposition == TreatmentFailureDisposition::RestoreFreshOwner
        {
            let reason = match &failure.kind {
                TreatmentErrorKind::Unconfirmed { reason } => *reason,
                TreatmentErrorKind::Renderer(_) => TreatmentConfirmationFailure::EvidenceLost,
            };
            self.revoke_treatment(reason);
        }
        let result = if let Some(failure) = observation.failure {
            Err(Box::new(failure))
        } else {
            let confirmation = TreatmentConfirmation::checked(
                observation.key,
                observation.playback,
                RendererConfirmation::checked(
                    observation.key,
                    observation.target.renderer.requested().clone(),
                ),
            );
            match confirmation {
                Ok(confirmation) => {
                    tracing::info!(
                        event_code = "treatment_confirmed",
                        apply_id = observation.key.apply.get(),
                        attempt_id = observation.key.attempt.get(),
                        pass = super::pass_token(observation.key.pass),
                    );
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
            && !self.treatment_poisoned
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
        #[cfg(test)]
        if clean {
            self.native_record(serde_json::json!({
                "event":"treatment_drain", "key":observation.key,
                "phase":phase_token(observation.phase), "index":observation.index,
                "pending":observation.pending,
            }));
        }
        let deadline_reached = now.duration_since(observation.admitted) >= TREATMENT_DEADLINE;
        if deadline_reached {
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
                        kind: RendererErrorKind::Rejected { .. }
                            | RendererErrorKind::Submission {
                                operation: RendererOperation::Set,
                                ..
                            },
                        ..
                    }),
                    ..
                })
            )
        {
            observation.lost(TreatmentConfirmationFailure::EvidenceLost);
        }
        if observation.failure.is_some() {
            // Preserve an unresolved SET cookie through this native drain so
            // a buffered scalar rejection can retain the definitive cause.
            // Proof is already revoked: no request or success can be issued.
            let awaiting_set_reply = !clean
                && !deadline_reached
                && observation.pending
                && observation.phase == Phase::Set;
            let settled = !awaiting_set_reply && (clean || observation.failure_without_drain);
            self.treatment_observation = Some(observation);
            if settled {
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
            && observation.phase == Phase::Verify
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
        let key = match pending {
            Pending::RendererRead { key, .. }
            | Pending::RendererSet { key, .. }
            | Pending::RendererPause { key, .. } => key,
            _ => return,
        };
        #[cfg(test)]
        self.native_record(serde_json::json!({
            "event":"renderer_reply", "key":key, "request":id, "kind":format!("{kind:?}"), "error":error,
            "property":name.and_then(|name| name.to_str().ok()),
            "value":format!("{value:?}")
        }));
        let Some(observation) = &mut self.treatment_observation else {
            return;
        };
        if key != observation.key {
            return;
        }
        tracing::info!(
            event_code = "renderer_owned_reply",
            apply_id = key.apply.get(),
            attempt_id = key.attempt.get(),
            pass = super::pass_token(key.pass),
            request_id = id,
            phase = phase_token(observation.phase),
            kind = super::reply_kind_token(kind),
            mpv_error = error,
        );
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
            &RendererSettings::default(),
        );
        let values = std::array::from_fn(|index| node(treatments.renderer.native()[index].1));
        let confirmation = crate::media::controller::test_support::treatment_confirmation(
            key(1, TreatmentPass::Open),
            playback,
            RendererSettings::default(),
        );
        backend.landed_treatments = Some((treatments, confirmation));
        backend.property_requests = Some(Vec::new());
        (backend, values)
    }

    fn answer(
        backend: &mut MpvBackend,
        current: &mut [NodeValue; 9],
        _now: Instant,
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
                .reply(id, ReplyKind::SetProperty, 0, None, None)
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
                .reply(
                    id,
                    ReplyKind::Property,
                    0,
                    Some(name.to_owned()),
                    Some(value),
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
                    BackendEvent::Observed { .. }
                    | BackendEvent::PauseObserved(_)
                    | BackendEvent::TreatmentFault { .. } => {}
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
    fn renderer_only_live_and_paused_verify_every_property_without_restarting() {
        for playback in [InitialPlayback::Live, InitialPlayback::Paused] {
            let (mut backend, mut current) = incumbent(playback);
            let target = crate::media::controller::test_support::prepared_treatments(&reference());
            backend
                .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
                .unwrap();
            let (result, requests) = settle(&mut backend, &mut current);
            let confirmation = result.unwrap();
            assert_eq!(confirmation.playback(), playback);
            assert_eq!(confirmation.key(), key(2, TreatmentPass::Candidate));
            assert_eq!(confirmation.renderer().settings(), &reference());
            assert_eq!(confirmation.renderer().key(), confirmation.key());
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
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
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
            .reply(
                id + 1,
                ReplyKind::Property,
                0,
                Some(c"scale".to_owned()),
                Some(current[0].clone()),
            )
            .unwrap();
        assert!(backend.treatment_observation.as_ref().unwrap().pending);
        backend
            .reply(id, ReplyKind::SetProperty, 0, None, None)
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
            let target = crate::media::controller::test_support::prepared_treatments(&reference());
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
                TreatmentFailureDisposition::RestoreFreshOwner
            );
            assert!(backend.landed_treatments.is_none());
            assert!(backend.treatment_poisoned);
        }
    }

    #[test]
    fn settled_second_write_failure_restores_the_whole_prior_once() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Paused);
        let prior = backend.landed_treatments.as_ref().unwrap().0.clone();
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
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
                    .reply(id, ReplyKind::SetProperty, -9, None, None)
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
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
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
            .treatment_tick(now + TREATMENT_DEADLINE, true)
            .unwrap();
        assert!(matches!(
            backend.emitted.pop_front(),
            Some(BackendEvent::TreatmentFault { .. })
        ));
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
        exhausted.last_internal = u64::MAX - (PASS_REQUESTS * 2 - 1);
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
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
        let target = crate::media::controller::test_support::prepared_treatments(&settings);
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
    }

    #[test]
    fn canonical_binary32_equality_is_exact_and_cancellation_wins_between_requests() {
        let compiled = crate::media::controller::test_support::prepared_treatments(&reference());
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
        assert!(matches!(
            backend.emitted.pop_front(),
            Some(BackendEvent::TreatmentFault { .. })
        ));
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
    fn known_proof_loss_is_sticky_and_refuses_before_native_work() {
        let (mut backend, _) = incumbent(InitialPlayback::Live);
        backend.revoke_treatment(TreatmentConfirmationFailure::EvidenceLost);
        assert!(matches!(
            backend.emitted.pop_front(),
            Some(BackendEvent::TreatmentFault { .. })
        ));
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
        let error = backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap_err();
        assert_eq!(error.code, "treatment_admission");
        assert!(backend.property_requests.as_ref().unwrap().is_empty());
        assert!(backend.pending.is_empty());
        assert!(backend.landed_treatments.is_none());
        assert!(backend.treatment_poisoned);
    }

    #[test]
    fn first_set_definitively_rejected_keeps_the_verified_prior_untouched() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let prior = current.clone();
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
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
            .reply(id, ReplyKind::SetProperty, -9, None, None)
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
    }

    // Write/0 is transient within the first SET tick. Use Snapshot/10 to
    // stop before that submission; the caller can then install a fault or
    // submit the SET and exercise its real native reply path.
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
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
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
            .treatment_tick(now + TREATMENT_DEADLINE, true)
            .unwrap();
        assert!(matches!(
            backend.emitted.pop_front(),
            Some(BackendEvent::TreatmentFault { .. })
        ));
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
        assert!(backend.landed_treatments.is_none());
    }

    #[test]
    fn native_loss_during_renderer_verification_revokes_prior_before_candidate_failure() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = advance_to(&mut backend, &mut current, Phase::Verify, 4);
        backend.native_log("overflow", "fatal", "log message buffer overflow: lost");
        backend.treatment_tick(now, false).unwrap();
        let events: Vec<_> = backend.emitted.drain(..).collect();
        assert!(
            matches!(&events[0], BackendEvent::TreatmentFault { key: applied, failure }
            if *applied == key(1, TreatmentPass::Open)
                && matches!(failure.kind, TreatmentErrorKind::Unconfirmed { reason: TreatmentConfirmationFailure::EvidenceLost }))
        );
        assert!(
            matches!(&events[1], BackendEvent::TreatmentResult { key: attempted, result: Err(failure), .. }
            if *attempted == key(2, TreatmentPass::Candidate)
                && failure.disposition == TreatmentFailureDisposition::RestoreFreshOwner)
        );
        assert_eq!(events.len(), 2);
        assert!(backend.landed_treatments.is_none());
        assert!(backend.treatment_observation.is_none());
        assert!(backend.pending.is_empty());
    }

    #[test]
    fn late_native_error_revokes_latest_renderer_identity_exactly_once() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        assert!(settle(&mut backend, &mut current).0.is_ok());
        for _ in 0..64 {
            backend.native_log("vo/gpu-next", "warn", "context only");
        }
        assert!(backend.emitted.is_empty());
        assert!(!backend.treatment_poisoned);
        for _ in 0..64 {
            backend.native_log("vo/gpu-next", "error", "native renderer unavailable");
        }
        let Some(BackendEvent::TreatmentFault {
            key: applied,
            failure,
        }) = backend.emitted.pop_front()
        else {
            panic!("latest confirmed renderer must be revoked");
        };
        assert_eq!(applied, key(2, TreatmentPass::Candidate));
        assert_eq!(failure.key, Some(applied));
        assert!(matches!(
            failure.kind,
            TreatmentErrorKind::Unconfirmed {
                reason: TreatmentConfirmationFailure::EvidenceLost
            }
        ));
        assert!(backend.emitted.is_empty());
        assert!(backend.treatment_poisoned);
        assert!(backend.landed_treatments.is_none());
    }

    #[test]
    fn sustained_nonclean_events_settle_renderer_deadline_without_faking_none() {
        let (mut backend, _) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
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
            .treatment_tick(now + TREATMENT_DEADLINE, false)
            .unwrap();
        assert!(matches!(
            backend.emitted.pop_front(),
            Some(BackendEvent::TreatmentFault { .. })
        ));
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
    }

    #[test]
    fn malformed_native_log_producer_ticks_and_settles_aggregate_without_none() {
        let (mut backend, _) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = backend.treatment_observation.as_ref().unwrap().admitted;
        let first = backend.malformed_log(now).unwrap();
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
                backend.malformed_log(now).unwrap(),
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
            let prior = crate::media::controller::test_support::prepared_treatments(&settings);
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
            assert!(matches!(
                backend.emitted.pop_front(),
                Some(BackendEvent::TreatmentFault { .. })
            ));
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
            assert!(backend.landed_treatments.is_none());
        }
    }

    #[test]
    fn insufficient_reservation_is_atomic_and_untouched_before_any_property_write() {
        let (mut backend, current) = incumbent(InitialPlayback::Live);
        let before = backend.landed_treatments.clone();
        backend.last_internal = u64::MAX - (PASS_REQUESTS * 2 - 1);
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
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
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = advance_to(&mut backend, &mut current, Phase::Write, 1);
        backend.treatment_tick(now, true).unwrap();
        let (id, _, _) = backend.property_requests.as_mut().unwrap().pop().unwrap();
        backend
            .reply(id, ReplyKind::SetProperty, -4, None, None)
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
    fn open_live_and_paused_read_all_nine_properties_and_pause_before_clean_drain() {
        for playback in [InitialPlayback::Live, InitialPlayback::Paused] {
            let (mut backend, mut current) = incumbent(playback);
            backend.landed_treatments = None;
            let target = crate::media::controller::test_support::prepared_treatments(
                &RendererSettings::default(),
            );
            let open = key(1, TreatmentPass::Open);
            backend.begin_treatment(7, open, target).unwrap();
            let now = advance_to(&mut backend, &mut current, Phase::Verify, 10);
            assert!(backend.emitted.is_empty());
            backend.treatment_tick(now, false).unwrap();
            assert!(backend.treatment_observation.is_some());
            assert!(backend.emitted.is_empty());
            backend.treatment_tick(now, true).unwrap();
            let Some(BackendEvent::TreatmentResult {
                result: Ok(confirmation),
                ..
            }) = backend.emitted.pop_front()
            else {
                panic!("complete Open requires actual final clean drain");
            };
            assert_eq!(confirmation.key(), open);
            assert_eq!(confirmation.playback(), playback);
            assert_eq!(confirmation.renderer().key(), open);
            assert!(backend.pending.is_empty());
        }
    }

    #[test]
    fn native_terminals_revoke_incumbent_then_candidate_before_terminal_at_every_boundary() {
        for event_id in [1, 7, 24] {
            for (phase, index) in [
                (Phase::Snapshot, 0),
                (Phase::Snapshot, 10),
                (Phase::Verify, 10),
            ] {
                let (mut backend, mut current) = incumbent(InitialPlayback::Live);
                let target =
                    crate::media::controller::test_support::prepared_treatments(&reference());
                backend
                    .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
                    .unwrap();
                let now = advance_to(&mut backend, &mut current, phase, index);
                if phase == Phase::Snapshot && index == 10 {
                    backend.treatment_tick(now, true).unwrap();
                    assert!(backend.treatment_observation.as_ref().unwrap().pending);
                }
                let mut end = MpvEndFile {
                    reason: 4,
                    error: -12,
                    playlist_entry_id: 0,
                    playlist_insert_id: 0,
                    playlist_insert_num_entries: 0,
                };
                let raw = MpvEvent {
                    event_id,
                    error: 0,
                    reply_userdata: 0,
                    data: if event_id == 7 {
                        ptr::from_mut(&mut end).cast()
                    } else {
                        ptr::null_mut()
                    },
                };
                // SAFETY: The native-layout event and optional END_FILE payload
                // remain live for this exact synchronous production decoder.
                let first = unsafe { backend.native_event(&raw) }.unwrap();
                let mut events = vec![first];
                events.extend(backend.emitted.drain(..));
                assert!(
                    matches!(&events[0], BackendEvent::TreatmentFault { key: applied, .. } if *applied == key(1, TreatmentPass::Open))
                );
                assert!(
                    matches!(&events[1], BackendEvent::TreatmentResult { result: Err(failure), .. }
                    if failure.disposition == TreatmentFailureDisposition::RestoreFreshOwner)
                );
                assert!(matches!(
                    (&events[2], event_id),
                    (BackendEvent::Shutdown, 1)
                        | (BackendEvent::EndFile { .. }, 7)
                        | (BackendEvent::QueueOverflow, 24)
                ));
                assert_eq!(events.len(), 3);
                assert!(backend.landed_treatments.is_none());
                assert!(backend.treatment_poisoned);
                assert!(backend.treatment_observation.is_none());
                assert!(backend.pending.is_empty());
            }
        }
    }

    #[test]
    fn known_set_rejection_owns_error_logs_in_both_orders_without_revoking_incumbent() {
        for logs_first in [false, true] {
            let (mut backend, mut current) = incumbent(InitialPlayback::Live);
            let prior = backend.landed_treatments.clone();
            let target = crate::media::controller::test_support::prepared_treatments(&reference());
            backend
                .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
                .unwrap();
            let now = advance_to(&mut backend, &mut current, Phase::Snapshot, 10);
            backend.treatment_tick(now, true).unwrap();
            let (id, _, _) = backend.property_requests.as_mut().unwrap().pop().unwrap();
            if !logs_first {
                backend
                    .reply(id, ReplyKind::SetProperty, -9, None, None)
                    .unwrap();
            }
            backend.native_log("vo/gpu-next", "error", "invalid native choice");
            backend.treatment_tick(now, false).unwrap();
            assert!(
                backend.emitted.is_empty(),
                "known rejection waits for a genuine clean log drain"
            );
            if logs_first {
                backend
                    .reply(id, ReplyKind::SetProperty, -9, None, None)
                    .unwrap();
            }
            backend.treatment_tick(now, true).unwrap();
            let Some(BackendEvent::TreatmentResult {
                result: Err(failure),
                ..
            }) = backend.emitted.pop_front()
            else {
                panic!("known rejection must preserve its exact native cause");
            };
            assert!(matches!(
                failure.kind,
                TreatmentErrorKind::Renderer(RendererFailure {
                    kind: RendererErrorKind::Rejected { mpv_error: -9 },
                    ..
                })
            ));
            assert_eq!(failure.disposition, TreatmentFailureDisposition::Untouched);
            assert_eq!(backend.landed_treatments, prior);
            assert!(!backend.treatment_poisoned);
            assert!(backend.emitted.is_empty());
        }
    }

    #[test]
    fn direct_set_submission_refusal_with_logs_keeps_prior_untouched() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let prior = backend.landed_treatments.clone();
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = advance_to(&mut backend, &mut current, Phase::Snapshot, 10);
        backend.renderer_submission_error = Some(-3);
        backend.treatment_tick(now, true).unwrap();
        backend.native_log("vo/gpu-next", "error", "native submission rejected");
        backend.treatment_tick(now, false).unwrap();
        assert!(backend.emitted.is_empty());
        backend.treatment_tick(now, true).unwrap();
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = backend.emitted.pop_front()
        else {
            panic!("submission refusal must remain nonfatal and typed");
        };
        assert!(matches!(
            failure.kind,
            TreatmentErrorKind::Renderer(RendererFailure {
                kind: RendererErrorKind::Submission {
                    operation: RendererOperation::Set,
                    mpv_error: -3
                },
                ..
            })
        ));
        assert_eq!(failure.disposition, TreatmentFailureDisposition::Untouched);
        assert_eq!(backend.landed_treatments, prior);
        assert!(!backend.treatment_poisoned);
        assert!(backend.property_requests.as_ref().unwrap().is_empty());
    }

    #[test]
    fn native_loss_plus_rejected_candidate_preserves_primary_cause_in_both_orders() {
        for loss_first in [false, true] {
            let (mut backend, mut current) = incumbent(InitialPlayback::Live);
            let target = crate::media::controller::test_support::prepared_treatments(&reference());
            backend
                .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
                .unwrap();
            let now = advance_to(&mut backend, &mut current, Phase::Snapshot, 10);
            backend.treatment_tick(now, true).unwrap();
            let (id, _, _) = backend.property_requests.as_mut().unwrap().pop().unwrap();
            let mut log = MpvLogMessage {
                prefix: c"overflow".as_ptr(),
                level: c"fatal".as_ptr(),
                text: c"log message buffer overflow: lost".as_ptr(),
                log_level: 10,
            };
            let loss = MpvEvent {
                event_id: 2,
                error: 0,
                reply_userdata: 0,
                data: ptr::from_mut(&mut log).cast(),
            };
            let rejection = MpvEvent {
                event_id: 4,
                error: -9,
                reply_userdata: id,
                data: ptr::null_mut(),
            };
            let order = if loss_first {
                [&loss, &rejection]
            } else {
                [&rejection, &loss]
            };
            let mut events = Vec::new();
            for raw in order {
                // SAFETY: The actual native event decoder receives live stack
                // layouts, including its ordinary non-clean post-event tick.
                let event = unsafe { backend.native_event(raw) }.unwrap();
                assert!(!matches!(
                    event,
                    BackendEvent::None | BackendEvent::TreatmentResult { result: Ok(_), .. }
                ));
                if !matches!(event, BackendEvent::Other) {
                    events.push(event);
                }
                events.extend(backend.emitted.drain(..));
                if loss_first && raw.event_id == 2 {
                    assert!(backend.treatment_observation.as_ref().unwrap().pending);
                    assert!(
                        backend
                            .pending
                            .iter()
                            .any(|(request, pending)| *request == id
                                && matches!(pending, Pending::RendererSet { .. }))
                    );
                    assert!(
                        events
                            .iter()
                            .all(|event| !matches!(event, BackendEvent::TreatmentResult { .. }))
                    );
                }
            }
            assert_eq!(events.len(), 2);
            assert!(
                matches!(&events[0], BackendEvent::TreatmentFault { key: applied, .. }
                if *applied == key(1, TreatmentPass::Open))
            );
            let BackendEvent::TreatmentResult {
                result: Err(failure),
                ..
            } = &events[1]
            else {
                panic!("both incumbent loss and exact candidate rejection remain observable");
            };
            assert!(matches!(
                failure.kind,
                TreatmentErrorKind::Renderer(RendererFailure {
                    kind: RendererErrorKind::Rejected { mpv_error: -9 },
                    ..
                })
            ));
            assert_eq!(
                failure.disposition,
                TreatmentFailureDisposition::RestoreFreshOwner
            );
            assert!(backend.landed_treatments.is_none());
            assert!(backend.treatment_poisoned);
            assert!(backend.emitted.is_empty());
        }
    }

    #[test]
    fn renderer_wrong_kind_command_payload_is_ignored_before_native_decode() {
        for (phase, index) in [
            (Phase::Snapshot, 0),
            (Phase::Snapshot, 9),
            (Phase::Snapshot, 10),
        ] {
            let (mut backend, mut current) = incumbent(InitialPlayback::Live);
            let target = crate::media::controller::test_support::prepared_treatments(&reference());
            backend
                .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
                .unwrap();
            let now = advance_to(&mut backend, &mut current, phase, index);
            backend.treatment_tick(now, true).unwrap();
            let id = backend.pending[0].0;
            let mut payload = MpvCommandEvent {
                result: MpvNode {
                    value: MpvNodeValue { int64: 0 },
                    format: 99,
                },
            };
            let raw = MpvEvent {
                event_id: 5,
                error: 0,
                reply_userdata: id,
                data: ptr::from_mut(&mut payload).cast(),
            };
            // SAFETY: The deliberately malformed native-layout command payload
            // stays live, but a property/SET cookie must reject its kind first.
            assert_eq!(
                unsafe { backend.native_event(&raw) }.unwrap(),
                BackendEvent::Other
            );
            assert_eq!(backend.pending[0].0, id);
            assert!(backend.treatment_observation.as_ref().unwrap().pending);
            assert!(!backend.treatment_poisoned);
            assert!(settle(&mut backend, &mut current).0.is_ok());
        }
    }

    #[test]
    fn malformed_native_log_payloads_revoke_proof_without_manufacturing_none() {
        for malformed in 0..3 {
            let (mut backend, _) = incumbent(InitialPlayback::Live);
            let target = crate::media::controller::test_support::prepared_treatments(&reference());
            backend
                .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
                .unwrap();
            let invalid = CString::new(vec![255]).unwrap();
            let mut log = MpvLogMessage {
                prefix: if malformed == 1 {
                    ptr::null()
                } else {
                    c"vo/gpu-next".as_ptr()
                },
                level: c"error".as_ptr(),
                text: invalid.as_ptr(),
                log_level: 20,
            };
            let raw = MpvEvent {
                event_id: 2,
                error: 0,
                reply_userdata: 0,
                data: if malformed == 0 {
                    ptr::null_mut()
                } else {
                    ptr::from_mut(&mut log).cast()
                },
            };
            // SAFETY: The deliberately invalid strings/nulls are checked by the
            // real native decoder while all nonnull stack pointers stay live.
            let first = unsafe { backend.native_event(&raw) }.unwrap();
            assert!(
                matches!(first, BackendEvent::TreatmentFault { key: applied, .. }
                if applied == key(1, TreatmentPass::Open))
            );
            let Some(BackendEvent::TreatmentResult {
                result: Err(failure),
                ..
            }) = backend.emitted.pop_front()
            else {
                panic!("malformed native log cannot authorize renderer proof");
            };
            assert!(matches!(
                failure.kind,
                TreatmentErrorKind::Unconfirmed {
                    reason: TreatmentConfirmationFailure::EvidenceLost,
                }
            ));
            assert_eq!(
                failure.disposition,
                TreatmentFailureDisposition::RestoreFreshOwner
            );
            assert!(backend.treatment_poisoned);
            assert!(backend.landed_treatments.is_none());
            assert!(backend.pending.is_empty());
            assert!(backend.emitted.is_empty());
        }
    }

    #[test]
    fn rejected_set_with_sustained_nonclean_flood_keeps_cause_and_loses_proof_at_deadline() {
        let (mut backend, mut current) = incumbent(InitialPlayback::Live);
        let target = crate::media::controller::test_support::prepared_treatments(&reference());
        backend
            .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
            .unwrap();
        let now = advance_to(&mut backend, &mut current, Phase::Snapshot, 10);
        backend.treatment_tick(now, true).unwrap();
        let (id, _, _) = backend.property_requests.as_mut().unwrap().pop().unwrap();
        backend
            .reply(id, ReplyKind::SetProperty, -9, None, None)
            .unwrap();
        for step in 1..100 {
            backend.native_log("vo/gpu-next", "warn", "bounded context");
            backend
                .treatment_tick(now + std::time::Duration::from_millis(step * 100), false)
                .unwrap();
            assert!(backend.emitted.is_empty());
        }
        backend
            .treatment_tick(now + TREATMENT_DEADLINE, false)
            .unwrap();
        assert!(matches!(
            backend.emitted.pop_front(),
            Some(BackendEvent::TreatmentFault { .. })
        ));
        let Some(BackendEvent::TreatmentResult {
            result: Err(failure),
            ..
        }) = backend.emitted.pop_front()
        else {
            panic!("the original deadline must settle without a fabricated clean drain");
        };
        assert!(matches!(
            failure.kind,
            TreatmentErrorKind::Renderer(RendererFailure {
                kind: RendererErrorKind::Rejected { mpv_error: -9 },
                ..
            })
        ));
        assert_eq!(
            failure.disposition,
            TreatmentFailureDisposition::RestoreFreshOwner
        );
        assert!(backend.landed_treatments.is_none());
        assert!(backend.treatment_poisoned);
        assert!(backend.pending.is_empty());
        assert!(backend.emitted.is_empty());
    }
    #[test]
    fn renderer_set_cookie_ignores_malformed_get_payload_before_exact_rejection() {
        for null_payload in [false, true] {
            let (mut backend, mut current) = incumbent(InitialPlayback::Live);
            let prior = backend.landed_treatments.clone();
            let target = crate::media::controller::test_support::prepared_treatments(&reference());
            backend
                .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
                .unwrap();
            let now = advance_to(&mut backend, &mut current, Phase::Snapshot, 10);
            backend.treatment_tick(now, true).unwrap();
            let (id, _, _) = backend.property_requests.as_mut().unwrap().pop().unwrap();
            let mut node = MpvNode {
                value: MpvNodeValue { int64: 0 },
                format: 99,
            };
            let mut payload = MpvPropertyEvent {
                name: c"scale".as_ptr(),
                format: 6,
                data: ptr::from_mut(&mut node).cast(),
            };
            let wrong_kind = MpvEvent {
                event_id: 3,
                error: 0,
                reply_userdata: id,
                data: if null_payload {
                    ptr::null_mut()
                } else {
                    ptr::from_mut(&mut payload).cast()
                },
            };
            // SAFETY: The actual GET producer must reject this SET cookie's
            // kind before inspecting the null or invalid native NODE payload.
            assert_eq!(
                unsafe { backend.native_event(&wrong_kind) }.unwrap(),
                BackendEvent::Other
            );
            assert!(backend.treatment_observation.as_ref().unwrap().pending);
            assert!(
                backend
                    .pending
                    .iter()
                    .any(|(request, pending)| *request == id
                        && matches!(pending, Pending::RendererSet { .. }))
            );
            assert!(!backend.treatment_poisoned);
            assert!(backend.emitted.is_empty());
            let rejection = MpvEvent {
                event_id: 4,
                error: -9,
                reply_userdata: id,
                data: ptr::null_mut(),
            };
            // SAFETY: SET replies use only the live event's exact scalar cookie.
            assert_eq!(
                unsafe { backend.native_event(&rejection) }.unwrap(),
                BackendEvent::Other
            );
            let none = MpvEvent {
                event_id: 0,
                error: 0,
                reply_userdata: 0,
                data: ptr::null_mut(),
            };
            // SAFETY: NONE owns no nested native payload.
            let BackendEvent::TreatmentResult {
                result: Err(failure),
                ..
            } = (unsafe { backend.native_event(&none) }).unwrap()
            else {
                panic!("the exact native rejection must survive wrong-kind GET traffic");
            };
            assert!(matches!(
                failure.kind,
                TreatmentErrorKind::Renderer(RendererFailure {
                    kind: RendererErrorKind::Rejected { mpv_error: -9 },
                    ..
                })
            ));
            assert_eq!(failure.disposition, TreatmentFailureDisposition::Untouched);
            assert_eq!(backend.landed_treatments, prior);
            assert!(!backend.treatment_poisoned);
            assert!(backend.pending.is_empty());
            assert!(backend.emitted.is_empty());
        }
    }

    #[test]
    fn lost_pending_set_stops_at_clean_drain_or_original_deadline_without_new_work() {
        for deadline in [false, true] {
            let (mut backend, mut current) = incumbent(InitialPlayback::Live);
            let target = crate::media::controller::test_support::prepared_treatments(&reference());
            backend
                .begin_treatment(7, key(2, TreatmentPass::Candidate), target)
                .unwrap();
            let now = advance_to(&mut backend, &mut current, Phase::Snapshot, 10);
            backend.treatment_tick(now, true).unwrap();
            let (id, _, _) = backend.property_requests.as_mut().unwrap().pop().unwrap();
            let mut log = MpvLogMessage {
                prefix: c"overflow".as_ptr(),
                level: c"fatal".as_ptr(),
                text: c"log message buffer overflow: lost".as_ptr(),
                log_level: 10,
            };
            let loss = MpvEvent {
                event_id: 2,
                error: 0,
                reply_userdata: 0,
                data: ptr::from_mut(&mut log).cast(),
            };
            // SAFETY: Genuine native log dispatch includes its post-event tick.
            assert_eq!(
                unsafe { backend.native_event(&loss) }.unwrap(),
                BackendEvent::Other
            );
            assert!(matches!(
                backend.emitted.pop_front(),
                Some(BackendEvent::TreatmentFault { .. })
            ));
            assert!(backend.emitted.is_empty());
            assert!(backend.pending.iter().any(|(request, _)| *request == id));
            let failure = if deadline {
                for step in 1..100 {
                    backend
                        .treatment_tick(now + std::time::Duration::from_millis(step * 100), false)
                        .unwrap();
                    assert!(backend.treatment_observation.is_some());
                    assert!(backend.emitted.is_empty());
                }
                backend
                    .treatment_tick(now + TREATMENT_DEADLINE, false)
                    .unwrap();
                let Some(BackendEvent::TreatmentResult {
                    result: Err(failure),
                    ..
                }) = backend.emitted.pop_front()
                else {
                    panic!("lost SET must settle at the original deadline under a non-clean flood");
                };
                failure
            } else {
                let none = MpvEvent {
                    event_id: 0,
                    error: 0,
                    reply_userdata: 0,
                    data: ptr::null_mut(),
                };
                // SAFETY: A genuine clean-drain event owns no nested payload.
                let BackendEvent::TreatmentResult {
                    result: Err(failure),
                    ..
                } = (unsafe { backend.native_event(&none) }).unwrap()
                else {
                    panic!("a clean drain ends the bounded correlation window");
                };
                failure
            };
            assert!(matches!(
                failure.kind,
                TreatmentErrorKind::Unconfirmed {
                    reason: TreatmentConfirmationFailure::EvidenceLost,
                }
            ));
            assert_eq!(
                failure.disposition,
                TreatmentFailureDisposition::RestoreFreshOwner
            );
            assert!(backend.treatment_poisoned);
            assert!(backend.landed_treatments.is_none());
            assert!(backend.treatment_observation.is_none());
            assert!(backend.pending.is_empty());
            assert!(backend.property_requests.as_ref().unwrap().is_empty());
            assert!(backend.emitted.is_empty());
        }
    }
}
