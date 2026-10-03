//! Requested capture facts and provenance-labeled decoder observations.
//! `container-fps` is a possibly inaccurate nominal report, not measured cadence.

pub use crate::capture::input::RequestedFacts;
use crate::domain::failure::{BackendEvidence, BackendOperation, Cause, Stage};
use crate::{capture::input::SelectionError, domain::capture::FrameSize};
use serde::Serialize;

pub const NOMINAL_RATE_RELATIVE_TOLERANCE: f64 = 0.005;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum Source {
    MpvDecodedParams,
    MpvContainerFps,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Observation<T> {
    pub value: T,
    pub source: Source,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ObservedFacts {
    pub decoded_size: Option<Observation<FrameSize>>,
    pub decoded_pixel_format: Option<Observation<String>>,
    pub nominal_rate: Option<Observation<f64>>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum VerificationStatus {
    Unverified,
    ObservedCompatible,
    Approximate,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Verification {
    pub captured_fourcc: VerificationStatus,
    pub decoded_size: VerificationStatus,
    pub nominal_rate: VerificationStatus,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionFacts {
    pub requested: RequestedFacts,
    pub observed: ObservedFacts,
    pub verification: Verification,
}

#[derive(Clone, Debug, Serialize, thiserror::Error)]
#[error("{stage:?}/{cause:?}: requested {requested}: {diagnostic}")]
pub struct SessionError {
    pub stage: Stage,
    pub cause: Cause,
    pub requested: RequestedFacts,
    pub diagnostic: String,
    pub evidence: Option<BackendEvidence>,
    #[source]
    #[serde(skip)]
    pub source: Option<std::sync::Arc<SelectionError>>,
}
impl PartialEq for SessionError {
    fn eq(&self, other: &Self) -> bool {
        self.stage == other.stage
            && self.cause == other.cause
            && self.requested == other.requested
            && self.diagnostic == other.diagnostic
            && self.evidence == other.evidence
    }
}
impl Eq for SessionError {}
impl SessionError {
    pub(crate) fn prevalidation(requested: RequestedFacts, error: SelectionError) -> Self {
        let stage = if matches!(
            &error,
            SelectionError::Input(
                crate::capture::input::InputError::UnmappedFourCc(_)
                    | crate::capture::input::InputError::UnrepresentablePath
                    | crate::capture::input::InputError::MalformedSpec(_)
            )
        ) {
            Stage::InputConstruction
        } else {
            Stage::Prevalidation
        };
        let cause = match &error {
            SelectionError::Capture(crate::capture::linux::CaptureError::PermissionDenied {
                ..
            }) => Cause::Permission,
            SelectionError::Capture(crate::capture::linux::CaptureError::Unsupported(_)) => {
                Cause::RequestedModeRefused
            }
            _ => Cause::Generic,
        };
        let mut diagnostic = error.to_string();
        if cause == Cause::Permission {
            diagnostic.push_str(
                "; read-only prevalidation denied: check node permissions and session ACLs",
            );
        }
        Self {
            stage,
            cause,
            requested,
            diagnostic,
            evidence: None,
            source: Some(std::sync::Arc::new(error)),
        }
    }
    pub fn backend(
        requested: RequestedFacts,
        diagnostic: String,
        evidence: Option<BackendEvidence>,
    ) -> Self {
        let (stage, cause) = match evidence
            .as_ref()
            .map(|proof| (proof.operation, proof.errno))
        {
            Some((BackendOperation::OpenDevice, 16)) => (Stage::Open, Cause::Busy),
            Some((BackendOperation::OpenDevice, 13 | 1)) => (Stage::Open, Cause::Permission),
            Some((BackendOperation::SetFormat | BackendOperation::SetFrameRate, 22)) => {
                (Stage::Negotiation, Cause::RequestedModeRefused)
            }
            Some((BackendOperation::StartStreaming, _)) => (Stage::StreamStart, Cause::Generic),
            _ => (Stage::Unknown, Cause::Generic),
        };
        let diagnostic = if cause == Cause::Permission {
            format!("{diagnostic}; check capture node permissions and session ACLs")
        } else {
            diagnostic
        };
        Self {
            stage,
            cause,
            requested,
            diagnostic,
            evidence,
            source: None,
        }
    }
    fn contradiction(requested: RequestedFacts, diagnostic: String) -> Self {
        Self {
            stage: Stage::Verification,
            cause: Cause::RequestedModeRefused,
            requested,
            diagnostic,
            evidence: None,
            source: None,
        }
    }
}

impl SessionFacts {
    #[allow(clippy::result_large_err)]
    pub fn verify(
        requested: RequestedFacts,
        mut observed: ObservedFacts,
    ) -> Result<Self, SessionError> {
        let decoded_size = match &observed.decoded_size {
            Some(observation) if observation.value != requested.mode.size => {
                return Err(SessionError::contradiction(
                    requested,
                    format!(
                        "MpvDecodedParams reports decoded {}x{}",
                        observation.value.width(),
                        observation.value.height()
                    ),
                ));
            }
            Some(_) => VerificationStatus::ObservedCompatible,
            None => VerificationStatus::Unverified,
        };
        let nominal_rate = match &observed.nominal_rate {
            Some(observation)
                if observation.source == Source::MpvContainerFps
                    && observation.value.is_finite()
                    && observation.value > 0.0 =>
            {
                let requested_rate = f64::from(requested.mode.rate.numerator())
                    / f64::from(requested.mode.rate.denominator());
                if ((observation.value - requested_rate) / requested_rate).abs()
                    > NOMINAL_RATE_RELATIVE_TOLERANCE
                {
                    return Err(SessionError::contradiction(
                        requested,
                        format!(
                            "MpvContainerFps nominal report {} contradicts requested rate (relative tolerance 0.5%; not measured cadence)",
                            observation.value
                        ),
                    ));
                }
                VerificationStatus::Approximate
            }
            _ => {
                // Non-finite or non-positive values are unavailable, not sentinel
                // capture rates and not serializable observations.
                observed.nominal_rate = None;
                VerificationStatus::Unverified
            }
        };
        Ok(Self {
            requested,
            observed,
            verification: Verification {
                captured_fourcc: VerificationStatus::Unverified,
                decoded_size,
                nominal_rate,
            },
        })
    }

    pub fn comparable_to(&self, other: &Self) -> bool {
        if self.requested != other.requested
            || self.observed.decoded_size != other.observed.decoded_size
            || self.observed.decoded_pixel_format != other.observed.decoded_pixel_format
        {
            return false;
        }
        match (&self.observed.nominal_rate, &other.observed.nominal_rate) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.source == b.source
                    && ((a.value - b.value) / a.value).abs() <= NOMINAL_RATE_RELATIVE_TOLERANCE
            }
            _ => false,
        }
    }

    pub fn summary(&self) -> String {
        let size = self
            .observed
            .decoded_size
            .as_ref()
            .map(|o| {
                format!(
                    "{}x{} (MpvDecodedParams)",
                    o.value.width(),
                    o.value.height()
                )
            })
            .unwrap_or_else(|| "unverified".into());
        let format = self
            .observed
            .decoded_pixel_format
            .as_ref()
            .map(|o| format!("{} (MpvDecodedParams)", o.value))
            .unwrap_or_else(|| "unverified".into());
        let rate = self
            .observed
            .nominal_rate
            .as_ref()
            .map(|o| {
                format!(
                    "{} nominal fps (MpvContainerFps; approximate, not measured)",
                    o.value
                )
            })
            .unwrap_or_else(|| "unverified".into());
        format!(
            "Requested: {}\nObserved decoded: {size}, {format}\nObserved rate: {rate}\nCaptured FourCC: unverified",
            self.requested
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capture::{input::CaptureSelection, linux},
        domain::capture::{CaptureMode, CapturedFourCc, FrameRate, FrameSize},
        media::controller::{
            BackendEvent, Generation, OwnerEndpoint, SubmitStatus, SurfaceToken, X11WindowId,
            test_support::{Config, Driver},
        },
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn mode() -> CaptureMode {
        CaptureMode {
            captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
            size: FrameSize::new(2560, 1440).unwrap(),
            rate: FrameRate::new(60, 1).unwrap(),
        }
    }
    fn selection() -> CaptureSelection {
        let args = crate::capture::input::CaptureArguments::parse(
            [
                "--capture-node",
                "/dev/video0",
                "--capture-fourcc",
                "NV12",
                "--capture-size",
                "2560x1440",
                "--capture-rate",
                "60/1",
            ]
            .map(std::ffi::OsString::from),
        )
        .unwrap()
        .unwrap();
        CaptureSelection::from_snapshot(
            &linux::session_fixture(&["/dev/video0"], args.mode),
            &args.node,
            args.mode,
        )
        .unwrap()
    }
    fn requested() -> RequestedFacts {
        selection().requested()
    }
    fn observed(width: u32, rate: Option<f64>) -> ObservedFacts {
        ObservedFacts {
            decoded_size: Some(Observation {
                value: FrameSize::new(width, 1440).unwrap(),
                source: Source::MpvDecodedParams,
            }),
            decoded_pixel_format: Some(Observation {
                value: "nv12".into(),
                source: Source::MpvDecodedParams,
            }),
            nominal_rate: rate.map(|value| Observation {
                value,
                source: Source::MpvContainerFps,
            }),
        }
    }
    fn start(generation: u64) -> (OwnerEndpoint, Driver) {
        let selection = selection();
        let requested = selection.requested();
        let (driver, backend) = Driver::pair(Config::default());
        let owner = OwnerEndpoint::spawn_capture_with_backend(
            Generation::new(generation).unwrap(),
            requested,
            move || selection.validate_snapshot(&linux::session_fixture(&["/dev/video0"], mode())),
            move |_| backend,
        )
        .unwrap();
        assert_eq!(
            owner.attach(SurfaceToken {
                generation: Generation::new(generation).unwrap(),
                xid: X11WindowId::new(42).unwrap()
            }),
            SubmitStatus::Accepted
        );
        driver.initialized.recv().unwrap();
        driver.submitted.recv().unwrap();
        (owner, driver)
    }
    fn close(owner: &mut OwnerEndpoint, driver: &Driver, generation: u64) {
        let generation = Generation::new(generation).unwrap();
        assert_eq!(owner.stop(generation, None), SubmitStatus::Accepted);
        owner.wait_for_ack().unwrap();
        let stopped = owner.take_stopped().unwrap().unwrap();
        assert_eq!(stopped.generation, generation);
        stopped.outcome.unwrap();
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn decoded_format_never_proves_capture_fourcc_and_absence_is_unverified() {
        let facts = SessionFacts::verify(requested(), observed(2560, None)).unwrap();
        assert_eq!(
            facts.verification.captured_fourcc,
            VerificationStatus::Unverified
        );
        assert_eq!(
            facts.verification.nominal_rate,
            VerificationStatus::Unverified
        );
        assert_eq!(
            facts.requested.mode.captured_fourcc,
            CapturedFourCc::from_bytes(*b"NV12")
        );
        assert_eq!(
            facts.observed.decoded_pixel_format.unwrap().source,
            Source::MpvDecodedParams
        );
        let empty = SessionFacts::verify(requested(), ObservedFacts::default()).unwrap();
        assert_eq!(
            empty.verification.decoded_size,
            VerificationStatus::Unverified
        );
    }

    #[test]
    fn nominal_rate_policy_rejects_contradiction_not_approximation_or_unknown() {
        let error = SessionFacts::verify(requested(), observed(2560, Some(30.0))).unwrap_err();
        assert_eq!(error.stage, Stage::Verification);
        assert_eq!(error.cause, Cause::RequestedModeRefused);
        assert_eq!(
            SessionFacts::verify(requested(), observed(2560, Some(59.94)))
                .unwrap()
                .verification
                .nominal_rate,
            VerificationStatus::Approximate
        );
        assert!(SessionFacts::verify(requested(), observed(2560, Some(60.3))).is_ok());
        assert!(SessionFacts::verify(requested(), observed(2560, Some(60.300001))).is_err());
        for value in [f64::NAN, f64::INFINITY, 0.0, -60.0] {
            assert_eq!(
                SessionFacts::verify(requested(), observed(2560, Some(value)))
                    .unwrap()
                    .verification
                    .nominal_rate,
                VerificationStatus::Unverified
            );
        }
        let error = SessionFacts::verify(requested(), observed(1920, Some(60.0))).unwrap_err();
        assert_eq!(error.stage, Stage::Verification);
        assert!(error.to_string().contains("2560x1440"));
    }

    #[test]
    fn generic_diagnostics_never_guess_errno_or_backend_stage() {
        let error = SessionError::backend(
            requested(),
            "open: Permission denied (13), device busy errno=16".into(),
            None,
        );
        assert_eq!((error.stage, error.cause), (Stage::Unknown, Cause::Generic));
        assert!(error.diagnostic.contains("Permission denied"));
        for (errno, cause) in [
            (16, Cause::Busy),
            (13, Cause::Permission),
            (1, Cause::Permission),
        ] {
            let error = SessionError::backend(
                requested(),
                "reported diagnostic".into(),
                Some(BackendEvidence {
                    operation: BackendOperation::OpenDevice,
                    errno,
                }),
            );
            assert_eq!((error.stage, error.cause), (Stage::Open, cause));
            assert_eq!(error.evidence.unwrap().errno, errno);
        }
        let error = SessionError::backend(
            requested(),
            "unknown operation".into(),
            Some(BackendEvidence {
                operation: BackendOperation::Unknown,
                errno: 13,
            }),
        );
        assert_eq!((error.stage, error.cause), (Stage::Unknown, Cause::Generic));
    }

    #[test]
    fn out_of_capability_prevalidation_never_constructs_backend() {
        let selection = selection();
        let requested = selection.requested();
        let factories = Arc::new(AtomicUsize::new(0));
        let factory_count = Arc::clone(&factories);
        let (driver, backend) = Driver::pair(Config::default());
        let mut owner = OwnerEndpoint::spawn_capture_with_backend(
            Generation::new(1).unwrap(),
            requested,
            move || {
                let unsupported = CaptureMode {
                    rate: FrameRate::new(30, 1).unwrap(),
                    ..mode()
                };
                selection.validate_snapshot(&linux::session_fixture(&["/dev/video0"], unsupported))
            },
            move |_| {
                factory_count.fetch_add(1, Ordering::SeqCst);
                backend
            },
        )
        .unwrap();
        owner.wait_for_ack().unwrap();
        let error = owner.take_stopped().unwrap().unwrap().outcome.unwrap_err();
        assert_eq!(error.session.unwrap().stage, Stage::Prevalidation);
        assert_eq!(factories.load(Ordering::SeqCst), 0);
        assert!(driver.initialized.try_recv().is_err());
        assert!(driver.submitted.try_recv().is_err());
    }

    #[test]
    fn file_loaded_not_ready_start_reads_fresh_metadata_late_reconfigure_stops() {
        let (mut owner, driver) = start(1);
        driver.set_observed(observed(1920, Some(30.0)));
        driver.send(BackendEvent::FileLoaded);
        driver.fence();
        let headers = owner.take_snapshot().unwrap();
        assert!(!headers.playback_started);
        assert!(headers.session.is_none());
        driver.set_observed(observed(2560, Some(59.94)));
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        let ready = owner.take_snapshot().unwrap();
        assert!(ready.playback_started);
        assert_eq!(
            ready.session.unwrap().verification.nominal_rate,
            VerificationStatus::Approximate
        );
        driver.set_observed(observed(1920, Some(60.0)));
        driver.send(BackendEvent::VideoReconfig);
        owner.wait_for_ack().unwrap();
        let error = owner.take_stopped().unwrap().unwrap().outcome.unwrap_err();
        assert_eq!(error.session.unwrap().stage, Stage::Verification);
        driver.destroyed.recv().unwrap();
    }

    #[test]
    fn same_tuple_reopens_with_comparable_observations_and_stale_intents_rejected() {
        let (mut first, first_driver) = start(1);
        first_driver.set_observed(observed(2560, Some(60.0)));
        first_driver.send(BackendEvent::PlaybackRestart);
        first_driver.fence();
        let first_facts = first.take_snapshot().unwrap().session.unwrap();
        close(&mut first, &first_driver, 1);
        let (mut second, second_driver) = start(2);
        second_driver.set_observed(observed(2560, Some(59.94)));
        second_driver.send(BackendEvent::PlaybackRestart);
        second_driver.fence();
        let second_facts = second.take_snapshot().unwrap().session.unwrap();
        assert_eq!(first_facts.requested, second_facts.requested);
        assert!(first_facts.comparable_to(&second_facts));
        assert_eq!(
            second.submit(
                Generation::new(1).unwrap(),
                crate::media::controller::PlaybackIntent::TogglePause
            ),
            SubmitStatus::StaleGeneration
        );
        close(&mut second, &second_driver, 2);
    }

    #[test]
    fn cancellation_during_worker_prevalidation_skips_backend_and_preserves_clean_ack() {
        let selection = selection();
        let requested = selection.requested();
        let factories = Arc::new(AtomicUsize::new(0));
        let factory_count = Arc::clone(&factories);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let gui_thread = std::thread::current().id();
        let (driver, backend) = Driver::pair(Config::default());
        let generation = Generation::new(1).unwrap();
        let mut owner = OwnerEndpoint::spawn_capture_with_backend(
            generation,
            requested,
            move || {
                entered_tx.send(std::thread::current().id()).unwrap();
                release_rx.recv().unwrap();
                selection.validate_snapshot(&linux::session_fixture(&["/dev/video0"], mode()))
            },
            move |_| {
                factory_count.fetch_add(1, Ordering::SeqCst);
                backend
            },
        )
        .unwrap();
        assert_ne!(entered_rx.recv().unwrap(), gui_thread);
        assert_eq!(owner.stop(generation, None), SubmitStatus::Accepted);
        release_tx.send(()).unwrap();
        owner.wait_for_ack().unwrap();
        owner.take_stopped().unwrap().unwrap().outcome.unwrap();
        assert_eq!(factories.load(Ordering::SeqCst), 0);
        assert!(driver.initialized.try_recv().is_err());
        assert!(driver.submitted.try_recv().is_err());
    }

    #[test]
    fn late_nominal_contradiction_stops_active_session_and_generic_refusal_never_retries() {
        let (mut owner, driver) = start(1);
        driver.set_observed(observed(2560, Some(60.0)));
        driver.send(BackendEvent::PlaybackRestart);
        driver.fence();
        assert!(owner.take_snapshot().unwrap().playback_started);
        driver.set_observed(observed(2560, Some(30.0)));
        driver.send(BackendEvent::VideoReconfig);
        owner.wait_for_ack().unwrap();
        let error = owner.take_stopped().unwrap().unwrap().outcome.unwrap_err();
        assert_eq!(error.session.unwrap().stage, Stage::Verification);
        driver.destroyed.recv().unwrap();
        assert!(driver.submitted.try_recv().is_err());

        let (mut owner, driver) = start(2);
        driver.send(BackendEvent::EndFile {
            reason: 4,
            error: -13,
        });
        owner.wait_for_ack().unwrap();
        let error = owner
            .take_stopped()
            .unwrap()
            .unwrap()
            .outcome
            .unwrap_err()
            .session
            .unwrap();
        assert_eq!((error.stage, error.cause), (Stage::Unknown, Cause::Generic));
        assert_eq!(error.requested, requested());
        assert!(error.diagnostic.contains("END_FILE"));
        driver.destroyed.recv().unwrap();
        assert!(driver.submitted.try_recv().is_err());
    }

    #[test]
    fn prevalidation_permission_retains_real_operation_without_backend_open_claim() {
        let source =
            crate::capture::input::SelectionError::Capture(linux::CaptureError::PermissionDenied {
                path: "/dev/video0".into(),
                operation: "open_read_only",
                source: std::io::Error::from_raw_os_error(13),
            });
        let error = SessionError::prevalidation(requested(), source);
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Prevalidation, Cause::Permission)
        );
        assert!(error.diagnostic.contains("open_read_only"));
        assert!(error.diagnostic.contains("ACL"));
        assert!(error.source.is_some());

        let mut wrong_source = observed(2560, Some(30.0));
        wrong_source.nominal_rate.as_mut().unwrap().source = Source::MpvDecodedParams;
        assert_eq!(
            SessionFacts::verify(requested(), wrong_source)
                .unwrap()
                .verification
                .nominal_rate,
            VerificationStatus::Unverified
        );
    }
}
