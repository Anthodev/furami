//! Publication receipts exercise actual health mutations and forced interleavings.
//! No worker scheduling assumptions, filesystem, global environment, or sleeps.
use super::*;
use crate::diagnostics::report::{StartupFailure, StartupTuple};

fn fixture(limits: LogLimits) -> (DiagnosticsHandle, Receiver<Message>) {
    build(DiagnosticsConfig {
        xdg_state_home: None,
        home: None,
        limits,
        logging_enabled: true,
    })
}
fn snapshot(handle: &DiagnosticsHandle, oversized: bool) -> ReportSnapshot {
    let mut report = ReportSnapshot {
        revision: handle.reserve_report_revision().unwrap(),
        ..Default::default()
    };
    report.session.phase = "Active".into();
    report.session.identity = SessionIdentity {
        operation: Some(91),
        attempt: Some(92),
        generation: Some(93),
    };
    report.session.projection_oversized = oversized;
    report
}
fn error(code: DiagnosticsCode) -> DiagnosticsError {
    DiagnosticsError::new(DiagnosticsOperation::Encode, code)
}
fn pause(handle: &DiagnosticsHandle, stage: PublicationStage) -> (Receiver<()>, mpsc::Sender<()>) {
    let (reached_tx, reached_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    *handle.shared.publication_pause.lock() = Some(PublicationPause {
        stage,
        reached: reached_tx,
        resume: resume_rx,
    });
    (reached_rx, resume_tx)
}
fn reached(receiver: &Receiver<()>) {
    receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("publication reached the exact mutation barrier");
}

#[test]
fn effective_limit_uses_validated_shared_configuration_not_health_input() {
    let (invalid, _receiver) = fixture(LogLimits {
        report_bytes: 1,
        ..LogLimits::default()
    });
    assert_eq!(invalid.health().limits.report_bytes, 1);
    assert_eq!(
        invalid.report_byte_limit(),
        LogLimits::default().report_bytes
    );
    let (small, _receiver) = fixture(LogLimits {
        report_bytes: 512,
        ..LogLimits::default()
    });
    assert_eq!(small.report_byte_limit(), 512);
}

#[test]
fn projection_refusal_emits_one_explicit_emergency_and_records_actual_own_mutations() {
    let (handle, receiver) = fixture(LogLimits::default());
    let before = handle.health();
    let report = snapshot(&handle, true);
    let revision = report.revision;
    let effects = handle.publish_report_with_effects(report);
    let after = handle.health();
    assert_eq!(
        effects.oversized_reports_added,
        after.oversized_reports - before.oversized_reports
    );
    assert_eq!(effects.oversized_reports_added, 1);
    let mut predicted = before;
    assert!(!effects.fold_into(&mut predicted));
    assert_eq!(predicted, after);
    let Message::Report {
        report: prepared, ..
    } = receiver.try_recv().unwrap()
    else {
        panic!("standalone report expected");
    };
    let line: serde_json::Value = serde_json::from_slice(&prepared.bytes).unwrap();
    assert_eq!(line["record_type"], "report");
    assert_eq!(line["report_revision"], revision.0);
    assert_eq!(line["report"]["incomplete"], true);
    assert_eq!(line["report"]["reason"], "OversizedReport");
    assert_eq!(line["report"]["identity"]["operation"], 91);
    assert_eq!(line["report"]["phase"], "Active");
    assert!(
        line["report"].get("environment").is_none(),
        "not a misleading empty/full report"
    );
}

#[test]
fn independent_folder_failure_is_not_absorbed_into_publication_prediction() {
    let (handle, _receiver) = fixture(LogLimits::default());
    let before = handle.health();
    let report = snapshot(&handle, true);
    let (entered, resume) = pause(&handle, PublicationStage::BeforeOversize);
    let effects = thread::scope(|scope| {
        let publisher = handle.clone();
        let pending = scope.spawn(move || publisher.publish_report_with_effects(report));
        reached(&entered);
        handle.reject_folder_counter_exhausted();
        resume.send(()).unwrap();
        pending.join().unwrap()
    });
    let after = handle.health();
    let mut predicted = before.clone();
    assert!(
        !effects.fold_into(&mut predicted),
        "folder changes do not manufacture an error-assignment history"
    );
    assert_eq!(predicted.folder, before.folder);
    assert_eq!(after.folder.state, FolderState::Failed);
    assert_ne!(
        predicted, after,
        "the independent folder change must remain detectable next poll"
    );
}

#[test]
fn independent_error_before_first_assignment_is_dirty_even_when_old_and_final_values_match() {
    let (handle, _receiver) = fixture(LogLimits::default());
    let own_error = error(DiagnosticsCode::OversizedReport);
    handle.shared.error(own_error);
    let before = handle.health();
    let report = snapshot(&handle, true);
    let (entered, resume) = pause(&handle, PublicationStage::BeforeOversize);
    let effects = thread::scope(|scope| {
        let publisher = handle.clone();
        let pending = scope.spawn(move || publisher.publish_report_with_effects(report));
        reached(&entered);
        handle.shared.error(DiagnosticsError::new(
            DiagnosticsOperation::Console,
            DiagnosticsCode::ConsoleWriteFailed,
        ));
        resume.send(()).unwrap();
        pending.join().unwrap()
    });
    assert_eq!(handle.health().last_error, before.last_error);
    let mut predicted = before;
    assert!(
        effects.fold_into(&mut predicted),
        "overwritten independent error must force the next report"
    );
    assert_eq!(predicted.last_error, Some(own_error));
}

#[test]
fn independent_error_between_oversize_and_queue_full_is_recorded_in_assignment_chain() {
    let (handle, _receiver) = fixture(LogLimits {
        queue_messages: 1,
        ..LogLimits::default()
    });
    handle.event(EventLevel::Info, "apply_runtime", &[]);
    let before = handle.health();
    let report = snapshot(&handle, true);
    let (entered, resume) = pause(&handle, PublicationStage::AfterOversize);
    let effects = thread::scope(|scope| {
        let publisher = handle.clone();
        let pending = scope.spawn(move || publisher.publish_report_with_effects(report));
        reached(&entered);
        handle.shared.error(DiagnosticsError::new(
            DiagnosticsOperation::Console,
            DiagnosticsCode::ConsoleWriteFailed,
        ));
        resume.send(()).unwrap();
        pending.join().unwrap()
    });
    assert!(effects.last_error.unwrap().interleaved);
    assert_eq!(
        handle.health().last_error.unwrap().code,
        DiagnosticsCode::QueueFull
    );
    let mut predicted = before;
    assert!(effects.fold_into(&mut predicted));
    assert_eq!(predicted.last_error, handle.health().last_error);
}

#[test]
fn independent_event_loss_and_storage_failure_survive_receipt_folding() {
    let (handle, _receiver) = fixture(LogLimits {
        queue_messages: 1,
        ..LogLimits::default()
    });
    handle.event(EventLevel::Info, "apply_runtime", &[]);
    let before = handle.health();
    let report = snapshot(&handle, true);
    let (entered, resume) = pause(&handle, PublicationStage::BeforeOversize);
    let effects = thread::scope(|scope| {
        let publisher = handle.clone();
        let pending = scope.spawn(move || publisher.publish_report_with_effects(report));
        reached(&entered);
        handle.event(EventLevel::Info, "apply_runtime", &[]);
        handle.shared.storage_error(DiagnosticsError::new(
            DiagnosticsOperation::Write,
            DiagnosticsCode::WriteFailed,
        ));
        resume.send(()).unwrap();
        pending.join().unwrap()
    });
    let after = handle.health();
    let mut predicted = before.clone();
    assert!(effects.fold_into(&mut predicted));
    assert_eq!(predicted.dropped_events, before.dropped_events);
    assert_eq!(predicted.sink_error, before.sink_error);
    assert_eq!(predicted.sink_status, before.sink_status);
    assert_eq!(after.dropped_events, before.dropped_events + 1);
    assert_eq!(after.sink_error.unwrap().code, DiagnosticsCode::WriteFailed);
    assert_ne!(predicted, after);
}

#[test]
fn another_publishers_oversize_increment_is_not_absorbed_into_own_counter_delta() {
    let (handle, _receiver) = fixture(LogLimits::default());
    let before = handle.health();
    let first = snapshot(&handle, true);
    let second = snapshot(&handle, true);
    let (entered, resume) = pause(&handle, PublicationStage::BeforeOversize);
    let effects = thread::scope(|scope| {
        let publisher = handle.clone();
        let pending = scope.spawn(move || publisher.publish_report_with_effects(first));
        reached(&entered);
        let _ = handle.publish_report_with_effects(second);
        resume.send(()).unwrap();
        pending.join().unwrap()
    });
    assert_eq!(effects.oversized_reports_added, 1);
    let mut predicted = before.clone();
    let _ = effects.fold_into(&mut predicted);
    assert_eq!(predicted.oversized_reports, before.oversized_reports + 1);
    assert_eq!(
        handle.health().oversized_reports,
        before.oversized_reports + 2
    );
    assert_eq!(
        handle.health().last_error.unwrap().code,
        DiagnosticsCode::InvalidReportRevision
    );
}

#[test]
fn saturation_records_zero_own_delta_after_an_independent_increment_consumes_the_last_slot() {
    let (handle, _receiver) = fixture(LogLimits::default());
    {
        let mut health = handle.shared.health.lock();
        health.oversized_reports = u64::MAX - 1;
        health.last_error = Some(error(DiagnosticsCode::OversizedReport));
    }
    let before = handle.health();
    let first = snapshot(&handle, true);
    let second = snapshot(&handle, true);
    let (entered, resume) = pause(&handle, PublicationStage::BeforeOversize);
    let effects = thread::scope(|scope| {
        let publisher = handle.clone();
        let pending = scope.spawn(move || publisher.publish_report_with_effects(first));
        reached(&entered);
        let _ = handle.publish_report_with_effects(second);
        resume.send(()).unwrap();
        pending.join().unwrap()
    });
    assert_eq!(effects.oversized_reports_added, 0);
    let mut predicted = before;
    let _ = effects.fold_into(&mut predicted);
    assert_eq!(predicted.oversized_reports, u64::MAX - 1);
    assert_eq!(handle.health().oversized_reports, u64::MAX);
    assert_ne!(
        predicted,
        handle.health(),
        "the independent saturated increment remains visible"
    );
}

#[test]
fn tiny_report_receipts_predict_only_own_feedback_without_creating_new_health_differences() {
    let (handle, _receiver) = fixture(LogLimits {
        report_bytes: 256,
        ..LogLimits::default()
    });
    let mut baseline = handle.health();
    for _ in 0..10 {
        let report = snapshot(&handle, false);
        let effects = handle.publish_report_with_effects(report);
        assert!(!effects.fold_into(&mut baseline));
        assert_eq!(baseline, handle.health());
    }
}

#[test]
fn independent_change_after_last_own_assignment_is_not_copied_from_post_health() {
    let (handle, _receiver) = fixture(LogLimits::default());
    let before = handle.health();
    let effects = handle.publish_report_with_effects(snapshot(&handle, true));
    handle.shared.error(DiagnosticsError::new(
        DiagnosticsOperation::Console,
        DiagnosticsCode::ConsoleWriteFailed,
    ));
    let mut predicted = before;
    assert!(!effects.fold_into(&mut predicted));
    assert_eq!(
        predicted.last_error.unwrap().code,
        DiagnosticsCode::OversizedReport
    );
    assert_eq!(
        handle.health().last_error.unwrap().code,
        DiagnosticsCode::ConsoleWriteFailed
    );
    assert_ne!(predicted, handle.health());
}

#[test]
fn publication_error_exits_report_actual_assignments_without_mutating_unrelated_fields() {
    for stopped in [false, true] {
        let (handle, receiver) = fixture(LogLimits::default());
        let report = snapshot(&handle, false);
        if stopped {
            handle.shared.accepting.store(false, Ordering::Release);
        } else {
            drop(receiver);
        }
        let before = handle.health();
        let effects = handle.publish_report_with_effects(report);
        let mut predicted = before;
        assert!(!effects.fold_into(&mut predicted));
        assert_eq!(predicted, handle.health());
        assert_eq!(
            predicted.last_error.unwrap().code,
            DiagnosticsCode::WorkerStopped
        );
    }
}

#[test]
fn post_encoding_shutdown_or_exhaustion_receipt_preserves_independent_fault() {
    for exhausted in [false, true] {
        let (handle, _receiver) = fixture(LogLimits::default());
        let report = snapshot(&handle, true);
        let before = handle.health();
        let (entered, resume) = pause(&handle, PublicationStage::BeforeAdmission);
        let effects = thread::scope(|scope| {
            let publisher = handle.clone();
            let pending = scope.spawn(move || publisher.publish_report_with_effects(report));
            reached(&entered);
            if exhausted {
                handle.counter_exhausted();
            } else {
                let _admission = handle.shared.admission.lock();
                handle.shared.accepting.store(false, Ordering::Release);
                handle.shared.storage_error(DiagnosticsError::new(
                    DiagnosticsOperation::Write,
                    DiagnosticsCode::WriteFailed,
                ));
            }
            resume.send(()).unwrap();
            pending.join().unwrap()
        });
        assert_eq!(effects.oversized_reports_added, 1);
        let mut predicted = before;
        assert!(effects.fold_into(&mut predicted));
        assert_eq!(
            predicted.last_error.unwrap().code,
            if exhausted {
                DiagnosticsCode::CounterExhausted
            } else {
                DiagnosticsCode::WorkerStopped
            }
        );
        assert_eq!(
            handle.shared.report.lock().revision,
            ReportRevision(0),
            "post-encode refusal must not admit the candidate"
        );
        if !exhausted {
            assert_eq!(
                handle.health().sink_error.unwrap().code,
                DiagnosticsCode::WriteFailed
            );
            assert_ne!(predicted, handle.health());
        }
    }
}

#[test]
fn invalid_revision_and_terminal_exhaustion_receipts_predict_exact_refusals() {
    for mode in 0..4 {
        let (handle, _receiver) = fixture(LogLimits::default());
        let mut report = snapshot(&handle, false);
        match mode {
            0 => report.revision = ReportRevision(0),
            1 => report.revision = ReportRevision(report.revision.0 + 1),
            2 => handle.publish_report(report.clone()),
            _ => handle.counter_exhausted(),
        }
        let before = handle.health();
        let effects = handle.publish_report_with_effects(report);
        assert_eq!(effects.oversized_reports_added, 0);
        let mut predicted = before;
        assert!(!effects.fold_into(&mut predicted));
        assert_eq!(predicted, handle.health());
        assert_eq!(
            predicted.last_error.unwrap().code,
            if mode == 3 {
                DiagnosticsCode::CounterExhausted
            } else {
                DiagnosticsCode::InvalidReportRevision
            }
        );
    }
}

#[test]
fn refused_emergency_replaces_old_identity_without_queuing_partial_json() {
    let (handle, receiver) = fixture(LogLimits {
        report_bytes: 256,
        ..LogLimits::default()
    });
    let mut previous = snapshot(&handle, true);
    previous.session.identity = SessionIdentity {
        operation: Some(11),
        attempt: Some(12),
        generation: Some(13),
    };
    let previous_revision = previous.revision;
    handle.publish_report(previous);
    let Message::Report {
        report: previous, ..
    } = receiver.try_recv().unwrap()
    else {
        panic!("previous context must be an admitted complete JSON checkpoint");
    };
    let previous_json: serde_json::Value = serde_json::from_slice(&previous.bytes).unwrap();
    assert_eq!(previous_json["report_revision"], previous_revision.0);
    assert_eq!(previous_json["report"]["identity"]["operation"], 11);

    let mut report = snapshot(&handle, true);
    // A minimal emergency fits this cap. Retain a real failure and requested
    // tuple so even the emergency cannot be admitted, without lowering the cap
    // below the validated configuration range.
    report.session.startup_failure = Some(StartupFailure {
        code: "CaptureSelection",
        stage: "Discovery",
        requested: Some(StartupTuple {
            format_policy: "Manual",
            fourcc: Some(0x3231564e),
            width: 1920,
            height: 1080,
            rate_numerator: 60000,
            rate_denominator: 1001,
        }),
    });
    let revision = report.revision;
    let identity = report.session.identity;
    let before = handle.health();
    let effects = handle.publish_report_with_effects(report);
    let mut predicted = before;
    assert_eq!(effects.oversized_reports_added, 1);
    assert!(!effects.fold_into(&mut predicted));
    assert_eq!(predicted, handle.health());
    assert_eq!(
        predicted.last_error.unwrap().code,
        DiagnosticsCode::OversizedReport
    );
    let current = handle.shared.report.lock();
    assert_eq!(current.revision, revision);
    assert_eq!(current.identity, identity);
    assert_ne!(current.revision, previous_revision);
    drop(current);
    assert!(receiver.try_recv().is_err());
    assert!(handle.shared.latest_report.lock().is_none());
    handle.event(EventLevel::Info, "apply_runtime", &[]);
    assert_eq!(handle.health().dropped_events, predicted.dropped_events + 1);
    assert_eq!(
        handle.health().last_error.unwrap().code,
        DiagnosticsCode::OversizedReport
    );
    assert!(
        receiver.try_recv().is_err(),
        "no event may claim the previous context"
    );
}

#[test]
fn disabled_publications_advance_current_revision_identity_without_output_or_loss_feedback() {
    let (handle, receiver) = build(DiagnosticsConfig {
        xdg_state_home: None,
        home: None,
        limits: LogLimits::default(),
        logging_enabled: false,
    });
    let before = handle.health();
    let bootstrap = Arc::clone(&handle.shared.report.lock());
    let bootstrap_bytes = bootstrap.bytes.clone();
    let mut predicted = before;
    for oversized in [false, true] {
        let mut report = snapshot(&handle, oversized);
        report.session.identity.generation = Some(report.revision.0 + 100);
        let revision = report.revision;
        let identity = report.session.identity;
        let effects = handle.publish_report_with_effects(report);
        assert_eq!(effects.oversized_reports_added, 0);
        assert!(!effects.fold_into(&mut predicted));
        assert_eq!(predicted, handle.health());
        let current = handle.shared.report.lock();
        assert_eq!(current.revision, revision);
        assert_eq!(current.identity, identity);
        assert!(!current.bytes.is_empty());
        assert!(receiver.try_recv().is_err());
        assert!(handle.shared.latest_report.lock().is_none());
    }
    assert_eq!(bootstrap.revision, ReportRevision(0));
    assert_eq!(bootstrap.bytes, bootstrap_bytes);
    handle.set_logging_enabled(true).unwrap();
    let current = Arc::clone(&handle.shared.report.lock());
    let (epoch, seeded) = handle.shared.latest_report.lock().take().unwrap();
    assert_ne!(epoch, 0);
    assert!(Arc::ptr_eq(&seeded, &current));
    let before = handle.health();
    let effects = handle.publish_report_with_effects(snapshot(&handle, true));
    let mut predicted = before;
    assert_eq!(effects.oversized_reports_added, 1);
    assert!(!effects.fold_into(&mut predicted));
    assert_eq!(predicted, handle.health());
}

#[test]
fn disabled_callbacks_skip_sequences_buffers_json_and_all_oversize_loss_accounting() {
    fn overflow_event() {
        tracing::info!(
            event_code = "apply_runtime",
            apply_id = 1_u64,
            attempt_id = 2_u64,
            generation = 3_u64,
            cleanup = 4_u64,
            cleanup_id = 5_u64,
            draft_revision = 6_u64,
            revision = 7_u64,
            request_id = 8_u64,
            sequence = 9_u64,
            ordinal = 10_u64,
            root_width = 11_u64,
            root_height = 12_u64,
            enabled = 13_u64,
            success = 14_u64,
            confirmed = 15_u64,
            video_width = 16_u64,
            video_height = 17_u64,
            truncated = 18_u64,
            fullscreen = 19_u64,
            mpv_error = 20_u64,
            errno = 21_u64,
            os_code = 22_u64,
            width = 23_u64,
            height = 24_u64,
            vendor_id = 25_u64,
            product_id = 26_u64,
            rate_numerator = 27_u64,
            rate_denominator = 28_u64,
            x = 29_u64,
            y = 30_u64,
            z = 31_u64,
            visible = 32_u64,
            active = 33_u64,
        );
    }
    let (handle, receiver) = build(DiagnosticsConfig {
        xdg_state_home: None,
        home: None,
        limits: LogLimits::default(),
        logging_enabled: false,
    });
    let native_oversize = "x".repeat(handle.shared.limits.event_bytes + 1);
    let fields = [SafeField::u64("apply_id", 1); MAX_FIELDS + 1];
    let before = handle.health();
    handle.event(EventLevel::Info, "apply_runtime", &fields);
    native_event(&handle, "qt_geometry", 1, &native_oversize);
    native_event(&handle, "qt_geometry", 1, "{invalid JSON");
    for severity in 0..=5 {
        qt_message(&handle, severity);
    }
    tracing::dispatcher::with_default(&handle.test_dispatch(), overflow_event);
    assert_eq!(handle.health(), before);
    assert_eq!(handle.shared.sequence.load(Ordering::Relaxed), 0);
    assert!(handle.shared.pool.lock().is_empty());
    assert!(receiver.try_recv().is_err());
    handle.set_logging_enabled(true).unwrap();
    handle.event(EventLevel::Info, "apply_runtime", &fields);
    native_event(&handle, "qt_geometry", 1, &native_oversize);
    tracing::dispatcher::with_default(&handle.test_dispatch(), overflow_event);
    let health = handle.health();
    assert_eq!(health.oversized_records, 3);
    assert_eq!(health.dropped_events, 3);
    assert!(health.has_problem());
    handle.set_logging_enabled(false).unwrap();
    assert!(!handle.health().has_problem());
    assert_eq!(handle.health().message(), "");
    handle.set_logging_enabled(true).unwrap();
    assert_eq!(handle.health().oversized_records, 3);
    assert_eq!(handle.health().dropped_events, 3);
}

#[test]
fn old_captured_producer_epoch_cannot_admit_or_account_rejection_after_off_on() {
    let (handle, receiver) = fixture(LogLimits::default());
    let old_epoch = handle.shared.recording_epoch.load(Ordering::Acquire);
    handle.set_logging_enabled(false).unwrap();
    handle.set_logging_enabled(true).unwrap();
    let before = handle.health();
    let sequence = handle.shared.sequence.load(Ordering::Relaxed);
    handle.submit(old_epoch, EventLevel::Info, "apply_runtime", &[]);
    assert!(!handle.reject_oversized_record(old_epoch));
    reject_native_record(&handle, old_epoch);
    assert_eq!(handle.health(), before);
    assert_eq!(handle.shared.sequence.load(Ordering::Relaxed), sequence);
    assert!(receiver.try_recv().is_err());
}

#[test]
fn repeated_enable_is_idempotent_and_epoch_exhaustion_never_reuses_a_period() {
    let (handle, receiver) = fixture(LogLimits::default());
    handle.event(EventLevel::Info, "apply_runtime", &[]);
    let epoch = handle.shared.recording_epoch.load(Ordering::Acquire);
    handle.set_logging_enabled(true).unwrap();
    assert_eq!(handle.shared.recording_epoch.load(Ordering::Acquire), epoch);
    let Message::Event {
        epoch: admitted, ..
    } = receiver.try_recv().unwrap()
    else {
        panic!("same-value enable retains admitted work");
    };
    assert_eq!(admitted, epoch);
    handle.set_logging_enabled(false).unwrap();
    handle.set_logging_enabled(false).unwrap();
    assert_eq!(handle.shared.admission.lock().last_logging_epoch, epoch);
    handle.shared.admission.lock().last_logging_epoch = u64::MAX - 1;
    handle.set_logging_enabled(true).unwrap();
    assert_eq!(
        handle.shared.recording_epoch.load(Ordering::Acquire),
        u64::MAX
    );
    handle.set_logging_enabled(false).unwrap();
    let before = handle.health();
    assert_eq!(
        handle.set_logging_enabled(true).unwrap_err().code,
        DiagnosticsCode::CounterExhausted
    );
    assert_eq!(handle.shared.recording_epoch.load(Ordering::Acquire), 0);
    assert_eq!(handle.health(), before);
    handle.set_logging_enabled(false).unwrap();
    handle.shared.accepting.store(false, Ordering::Release);
    assert_eq!(
        handle.set_logging_enabled(true).unwrap_err().code,
        DiagnosticsCode::WorkerStopped
    );
    handle.set_logging_enabled(false).unwrap();
    assert!(!handle.health().logging_enabled);
}

#[test]
fn mode_change_during_publication_is_independent_health_not_fabricated_own_feedback() {
    let (handle, receiver) = fixture(LogLimits::default());
    let report = snapshot(&handle, false);
    let revision = report.revision;
    let before = handle.health();
    let (entered, resume) = pause(&handle, PublicationStage::BeforeAdmission);
    let effects = thread::scope(|scope| {
        let publisher = handle.clone();
        let pending = scope.spawn(move || publisher.publish_report_with_effects(report));
        reached(&entered);
        handle.set_logging_enabled(false).unwrap();
        resume.send(()).unwrap();
        pending.join().unwrap()
    });
    let mut predicted = before;
    assert!(!effects.fold_into(&mut predicted));
    assert!(predicted.logging_enabled);
    assert!(!handle.health().logging_enabled);
    assert_ne!(predicted, handle.health());
    assert_eq!(handle.shared.report.lock().revision, revision);
    assert!(receiver.try_recv().is_err());
    assert!(handle.shared.latest_report.lock().is_none());
}

#[test]
fn oversize_publication_cancelled_before_accounting_updates_cache_without_loss() {
    let (handle, receiver) = fixture(LogLimits::default());
    let report = snapshot(&handle, true);
    let revision = report.revision;
    let (entered, resume) = pause(&handle, PublicationStage::BeforeOversize);
    let effects = thread::scope(|scope| {
        let publisher = handle.clone();
        let pending = scope.spawn(move || publisher.publish_report_with_effects(report));
        reached(&entered);
        handle.set_logging_enabled(false).unwrap();
        resume.send(()).unwrap();
        pending.join().unwrap()
    });
    assert_eq!(effects.oversized_reports_added, 0);
    assert!(effects.last_error.is_none());
    assert_eq!(handle.shared.report.lock().revision, revision);
    assert_eq!(handle.health().oversized_reports, 0);
    assert!(receiver.try_recv().is_err());
}

#[test]
fn disabled_tiny_bootstrap_and_emergency_report_do_not_invent_output_loss() {
    let (handle, receiver) = build(DiagnosticsConfig {
        xdg_state_home: None,
        home: None,
        limits: LogLimits {
            report_bytes: 256,
            ..LogLimits::default()
        },
        logging_enabled: false,
    });
    assert_eq!(handle.health().oversized_reports, 0);
    let before = handle.health();
    let report = snapshot(&handle, true);
    let revision = report.revision;
    let effects = handle.publish_report_with_effects(report);
    let mut predicted = before;
    assert_eq!(effects.oversized_reports_added, 0);
    assert!(!effects.fold_into(&mut predicted));
    assert_eq!(predicted, handle.health());
    assert_eq!(handle.shared.report.lock().revision, revision);
    assert!(handle.shared.latest_report.lock().is_none());
    assert!(receiver.try_recv().is_err());
    handle.shared.accepting.store(false, Ordering::Release);
    handle.event(EventLevel::Info, "apply_runtime", &[]);
    assert_eq!(
        predicted,
        handle.health(),
        "off events do not report terminal-worker loss"
    );
}
