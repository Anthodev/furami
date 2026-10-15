use super::*;
use crate::domain::{capture::*, failure::*, state::*};

fn settings() -> DraftSettings {
    let mode = CaptureMode {
        captured_fourcc: CapturedFourCc::from_bytes(*b"NV12"),
        size: FrameSize::new(1920, 1080).unwrap(),
        rate: FrameRate::new(60000, 1001).unwrap(),
    };
    let identity = crate::capture::linux::session_fixture(&["/dev/video-private"], mode).devices()
        [0]
    .identity()
    .clone();
    DraftSettings {
        video: VideoRequest::manual(ModeRequest { identity, mode }),
        audio: AudioSelection::default(),
        renderer: Default::default(),
    }
}
fn key() -> AttemptKey {
    AttemptKey {
        apply: ApplyId::new(7).unwrap(),
        attempt: AttemptId::new(9).unwrap(),
        purpose: AttemptPurpose::Candidate,
    }
}
fn sink(name: &str) -> crate::domain::output::LiveSinkTarget {
    crate::domain::output::LiveSinkTarget::new(
        crate::domain::output::SinkIdentity::new(
            name.into(),
            vec![("device.serial".into(), "private-sink-property".into())],
        )
        .unwrap(),
        std::num::NonZeroU64::new(17).unwrap(),
        18,
    )
    .unwrap()
}
#[test]
fn audio_choice_and_confirmed_route_are_distinct_and_properties_never_serialize() {
    use crate::domain::output::*;
    let selected = PersistentOutputChoice::Manual(sink("desired-output").identity);
    let plan = OutputPlan::Target {
        revision: OutputRevision::first(),
        target: sink("desired-output"),
    };
    let source = AudioSourceIdentity::new(
        "technical-input".into(),
        vec![("device.serial".into(), "private-source-property".into())],
    )
    .unwrap();
    let route = crate::media::loopback::LoopbackReceipt::for_test(
        crate::media::controller::Generation::new(9).unwrap(),
        key().attempt,
        AudioEpoch::new(3).unwrap(),
        WatchStamp {
            watch: WatchId::new(4).unwrap(),
            epoch: ObservationEpoch::new(5).unwrap(),
        },
        source,
        sink("actual-output"),
        OutputRevision::first(),
    );
    let report = AudioProjection::new(
        &selected,
        &plan,
        Some(&AudioAvailability::Active { route }),
        HelperReport::default(),
    )
    .into_report();
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["selected_output"]["technical_name"], "desired-output");
    assert_eq!(
        json["effective_route"]["value"]["sink_technical_name"],
        "actual-output"
    );
    assert_eq!(
        json["effective_route"]["value"]["source_technical_name"],
        "technical-input"
    );
    let text = json.to_string();
    assert!(!text.contains("private-"));
    assert!(!text.contains("stable_properties"));
    assert!(matches!(
        AudioProjection::new(
            &PersistentOutputChoice::Auto,
            &plan,
            None,
            HelperReport::default()
        )
        .into_report()
        .selected_output,
        SelectedOutput::Auto
    ));
}
#[test]
fn audio_silences_keep_all_codes_without_promoting_plan_or_error_details() {
    use crate::domain::output::*;
    let silences = [
        (
            AudioSilence::WaitingForSource(AudioError::Control("private-error".into())),
            AudioCode::WaitingForSource,
        ),
        (AudioSilence::PendingRoute, AudioCode::PendingRoute),
        (AudioSilence::Paused, AudioCode::Paused),
        (
            AudioSilence::Failed(AudioError::Control("private-error".into())),
            AudioCode::Failed,
        ),
        (
            AudioSilence::Output(OutputSilence::NoAvailableOutput),
            AudioCode::NoAvailableOutput,
        ),
        (
            AudioSilence::Output(OutputSilence::CatalogUnavailable("private-error".into())),
            AudioCode::CatalogUnavailable,
        ),
        (
            AudioSilence::Output(OutputSilence::ManualUnavailable),
            AudioCode::ManualUnavailable,
        ),
        (
            AudioSilence::Output(OutputSilence::ManualRequiresAction),
            AudioCode::ManualRequiresAction,
        ),
        (
            AudioSilence::Output(OutputSilence::RoutingConflict("private-error".into())),
            AudioCode::RoutingConflict,
        ),
    ];
    let plan = OutputPlan::Silent {
        revision: OutputRevision::first(),
        reason: OutputSilence::RoutingConflict("private-plan".into()),
    };
    for (reason, expected) in silences {
        let report = AudioProjection::new(
            &PersistentOutputChoice::Auto,
            &plan,
            Some(&AudioAvailability::Silent { reason }),
            HelperReport::default(),
        )
        .into_report();
        assert!(
            matches!(report.availability, Observation::Observed { value } if value == expected)
        );
        assert!(matches!(
            report.effective_route,
            Observation::Unavailable { .. }
        ));
        assert!(!serde_json::to_string(&report).unwrap().contains("private-"));
    }
    let no_owner = AudioProjection::new(
        &PersistentOutputChoice::Auto,
        &plan,
        None,
        HelperReport::default(),
    )
    .into_report();
    assert!(matches!(
        no_owner.availability,
        Observation::Unavailable { .. }
    ));
    assert_eq!(no_owner.plan_reason, Some(AudioCode::RoutingConflict));
}
#[test]
fn timing_retains_exact_request_actual_source_value_and_never_invents_cadence() {
    use crate::{
        app::ports::FactStatus,
        media::{
            gate::{PlaybackTimingFact, ReceiptTimingFacts},
            session::Source,
        },
    };
    for (status, source, expected) in [
        (
            FactStatus::Configured,
            Source::MpvConfiguredContainerFps,
            "Configured",
        ),
        (
            FactStatus::Approximate,
            Source::MpvContainerFps,
            "Approximate",
        ),
    ] {
        let facts = ReceiptTimingFacts {
            key: key(),
            generation: crate::media::controller::Generation::new(9).unwrap(),
            requested: FrameRate::new(60000, 1001).unwrap(),
            status,
            observation: Some(PlaybackTimingFact {
                value: 59.5,
                source,
            }),
        };
        let json = serde_json::to_value(TimingReport::from_receipt(&facts)).unwrap();
        assert_eq!(json["requested"]["value"]["numerator"], 60000);
        assert_eq!(json["requested"]["value"]["denominator"], 1001);
        assert_eq!(json["playback"]["value"]["value"], 59.5);
        assert_eq!(json["playback"]["value"]["status"], expected);
        assert_ne!(
            json["derived_configured_value"]["value"],
            json["playback"]["value"]["value"]
        );
        assert_eq!(json["measured_capture_cadence"]["reason"], "NotMeasured");
        let missing = TimingReport::from_receipt(&ReceiptTimingFacts {
            status: FactStatus::Unverified,
            observation: None,
            ..facts
        });
        assert!(matches!(missing.playback, Observation::Unavailable { .. }));
    }
    assert!(matches!(
        TimingReport::default().requested,
        Observation::Unavailable { .. }
    ));
}
#[test]
fn backend_projection_reports_only_actual_owner_values_or_typed_unavailability() {
    use crate::media::controller::{
        BackendObservation, BackendUnavailable, RuntimeMediaObservation,
    };
    let mut observation = RuntimeMediaObservation {
        generation: 9,
        observation_epoch: 3,
        client_api: 0x0002_0005,
        libmpv_version: "mpv 0.41.0".into(),
        backend: BackendObservation::Observed {
            video_output: "actual-vo".into(),
            gpu_context: "actual-context".into(),
        },
        helpers: Default::default(),
    };
    let report = RuntimeMediaReport::from(&observation);
    let json = serde_json::to_value(report).unwrap();
    assert_eq!(json["backend"]["value"]["video_output"], "actual-vo");
    assert_eq!(json["client_api"]["value"], 0x0002_0005);
    observation.backend = BackendObservation::Unavailable(BackendUnavailable::Invalidated);
    let text = serde_json::to_string(&RuntimeMediaReport::from(&observation)).unwrap();
    assert!(!text.contains("actual-vo"));
    assert!(text.contains("Invalidated"));
    assert!(matches!(
        RuntimeMediaReport::default().backend,
        Observation::Unavailable { .. }
    ));
}
#[test]
fn serialized_failure_lanes_keep_frozen_tuple_phase_and_codes_but_no_private_payload() {
    use crate::app::diagnostic::*;
    let mut requested = settings();
    requested.video.selected_node = Some("/dev/video-private-node".into());
    requested.video.identity = DeviceIdentity::new(
        0x32ed,
        0x3701,
        UsbTopology::new(
            "private-controller".into(),
            vec![std::num::NonZeroU8::new(1).unwrap()],
        )
        .unwrap(),
        Some("private-serial".into()),
    )
    .unwrap();
    let model = ProductModel::new(requested.clone());
    let treatment_key = TreatmentAttemptKey {
        apply: key().apply,
        attempt: key().attempt,
        pass: TreatmentPass::Candidate,
    };
    let error = ApplyFailure::new(
        FailureCategory::Session,
        Stage::Open,
        Cause::Busy,
        requested,
        "private-operation-/home/me",
        "private-native-error-/dev/video-private-node",
    )
    .with_evidence(Some(BackendEvidence {
        operation: BackendOperation::OpenDevice,
        errno: 16,
    }))
    .with_treatment(TreatmentFailure {
        key: Some(treatment_key),
        disposition: TreatmentFailureDisposition::RestoreFreshOwner,
        kind: TreatmentErrorKind::Unconfirmed {
            reason: TreatmentConfirmationFailure::EvidenceLost,
        },
    });
    let context = std::sync::Arc::new(DiagnosticFailureContext {
        identity: model.state_identity(),
        correlation: DiagnosticFailureCorrelation::Attempt(key()),
        error: (&error).into(),
    });
    let mut renderer_error = error.clone();
    renderer_error.treatment = Some(Box::new(TreatmentFailure {
        key: Some(treatment_key),
        disposition: TreatmentFailureDisposition::RestoreSameOwner,
        kind: TreatmentErrorKind::Renderer(RendererFailure {
            property: crate::domain::render::RendererProperty::Scale,
            kind: RendererErrorKind::ReadbackMismatch,
            diagnostic: "private-renderer-native-error".into(),
        }),
    }));
    let renderer_context = std::sync::Arc::new(DiagnosticFailureContext {
        identity: model.state_identity(),
        correlation: DiagnosticFailureCorrelation::Treatment(treatment_key),
        error: (&renderer_error).into(),
    });
    let internal = DiagnosticFailureContexts {
        validation: Some(context.clone()),
        candidate: Some(context.clone()),
        restore: Some(renderer_context),
        resume: Some(context.clone()),
        incumbent: Some(context.clone()),
        cleanup: std::array::from_fn(|_| Some(context.clone())),
    };
    let mut report = ReportSnapshot::initial();
    report.session.phase = "ErrorWithActiveRestored".into();
    report.session.failures = (&internal).into();
    let json = serde_json::to_value(&report).unwrap();
    let text = json.to_string();
    assert!(!text.contains("private-"));
    assert!(!text.contains("selected_node"));
    assert!(!text.contains("topology"));
    assert_eq!(
        json["session"]["failures"]["candidate"]["requested"]["requested"]["rate_denominator"],
        1001
    );
    assert_eq!(
        json["session"]["failures"]["candidate"]["phase_at_failure"],
        "Stopped"
    );
    assert_eq!(json["session"]["phase"], "ErrorWithActiveRestored");
    assert_eq!(
        json["session"]["failures"]["restore"]["evidence"]["errno"],
        16
    );
    assert_eq!(
        json["session"]["failures"]["incumbent"]["operation"],
        "Other"
    );
    assert_eq!(
        json["session"]["failures"]["candidate"]["treatment"]["error"]["reason"],
        "EvidenceLost"
    );
    assert_eq!(
        json["session"]["failures"]["restore"]["treatment"]["error"]["code"],
        "ReadbackMismatch"
    );
    let emergency = serde_json::to_string(&report.emergency()).unwrap();
    assert!(!emergency.contains("private-"));
    assert!(emergency.contains("candidate"));
    assert!(emergency.contains("60000"));
    let emergency = serde_json::to_value(report.emergency()).unwrap();
    for lane in ["validation", "candidate", "restore", "resume", "incumbent"] {
        assert_eq!(
            emergency["failures"][lane]["requested"]["requested"]["rate_numerator"],
            60000
        );
        assert_eq!(
            emergency["failures"][lane]["requested"]["requested"]["rate_denominator"],
            1001
        );
    }
    assert_eq!(
        emergency["failures"]["cleanup"].as_array().unwrap().len(),
        6
    );
}
#[test]
fn renderer_projection_retains_every_native_value_without_private_payload() {
    use crate::domain::render::*;
    let settings = RendererSettings::new(RendererValues {
        scale: ScalerKernel::Lanczos,
        cscale: ScalerChoice::Inherit,
        dscale: ScalerChoice::Kernel(ScalerKernel::Hermite),
        scale_antiring: 0.75,
        dither: DitherMode::Ordered,
        deband: DebandValues {
            enabled: true,
            iterations: 3,
            grain: 42.0,
        },
        interpolation: true,
    })
    .unwrap();
    let json = serde_json::to_value(RendererReport::from(&settings)).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 9);
    assert_eq!(json["scale_antiring"], 0.75);
    assert_eq!(json["deband_iterations"], 3);
    assert_eq!(json["deband_grain"], 42.0);
    assert_eq!(json["interpolation"], true);
    assert_eq!(
        expected_card_model(0x32ed, 0x3701),
        Some(ExpectedCardModel::GenkiShadowCast3)
    );
    assert_eq!(expected_card_model(0x32ed, 0x3702), None);
}

#[test]
fn borrowed_audio_comparison_ignores_private_error_and_route_property_payloads() {
    use crate::domain::output::*;
    let choice = PersistentOutputChoice::Auto;
    let plan = OutputPlan::Silent {
        revision: OutputRevision::first(),
        reason: OutputSilence::NoAvailableOutput,
    };
    let first = AudioAvailability::Silent {
        reason: AudioSilence::WaitingForSource(AudioError::Control("first-private-error".into())),
    };
    let cached =
        AudioProjection::new(&choice, &plan, Some(&first), HelperReport::default()).into_report();
    let oversized = AudioAvailability::Silent {
        reason: AudioSilence::WaitingForSource(AudioError::Control("private-error".repeat(65_536))),
    };
    assert!(
        AudioProjection::new(&choice, &plan, Some(&oversized), HelperReport::default())
            .matches(&cached)
    );
    let source = |private: String| {
        AudioSourceIdentity::new(
            "technical-input".into(),
            vec![("device.serial".into(), private)],
        )
        .unwrap()
    };
    let route = |source| {
        crate::media::loopback::LoopbackReceipt::for_test(
            crate::media::controller::Generation::new(9).unwrap(),
            key().attempt,
            AudioEpoch::new(3).unwrap(),
            WatchStamp {
                watch: WatchId::new(4).unwrap(),
                epoch: ObservationEpoch::new(5).unwrap(),
            },
            source,
            sink("actual-output"),
            OutputRevision::first(),
        )
    };
    let first = AudioAvailability::Active {
        route: route(source("private-first".into())),
    };
    let cached =
        AudioProjection::new(&choice, &plan, Some(&first), HelperReport::default()).into_report();
    let oversized = AudioAvailability::Active {
        route: route(source("private-property-value".repeat(65_536))),
    };
    assert!(
        AudioProjection::new(&choice, &plan, Some(&oversized), HelperReport::default())
            .matches(&cached)
    );
    assert!(!serde_json::to_string(&cached).unwrap().contains("private-"));
}
