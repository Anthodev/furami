//! Behavioral coverage for bounded diagnostics storage, privacy and admission.
use super::{
    logging::{self, Failpoint, Worker},
    report::*,
    *,
};
use parking_lot::Mutex;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, BufReader, Write},
    os::unix::{
        ffi::OsStringExt,
        fs::{MetadataExt, symlink},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "furami-diagnostics-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn config(&self) -> DiagnosticsConfig {
        DiagnosticsConfig {
            xdg_state_home: Some(self.0.clone().into_os_string()),
            home: None,
            limits: limits(),
            logging_enabled: true,
        }
    }
    fn logs(&self) -> PathBuf {
        self.0.join("furami/logs")
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn limits() -> LogLimits {
    LogLimits {
        file_bytes: 65_536,
        files: 5,
        event_bytes: 2_048,
        report_bytes: 49_152,
        queue_messages: 64,
    }
}
fn report(revision: u64, phase: &str) -> ReportSnapshot {
    let mut report = ReportSnapshot {
        revision: ReportRevision(revision),
        ..Default::default()
    };
    report.session.phase = phase.into();
    report
}
fn publish(handle: &DiagnosticsHandle, report: ReportSnapshot) {
    let reserved = handle
        .reserve_report_revision()
        .expect("test revision space available");
    assert_eq!(
        reserved, report.revision,
        "fixture expected the next shared revision"
    );
    handle.publish_report(report);
}
#[derive(Clone, Default)]
struct Console(Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>);
impl Write for Console {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.1.load(Ordering::Relaxed) {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Harness {
    handle: DiagnosticsHandle,
    worker: Worker,
    console: Console,
}
impl Harness {
    fn new(config: DiagnosticsConfig) -> Self {
        let console = Console::default();
        let (handle, worker) = logging::test_pair(config, Box::new(console.clone()));
        Self {
            handle,
            worker,
            console,
        }
    }
    fn drain(&mut self) {
        self.worker.drain_available();
    }
    fn event(&self, sequence: u64) {
        self.handle.event(
            EventLevel::Info,
            "apply_runtime",
            &[SafeField::u64("apply_id", sequence)],
        );
    }
}
fn records(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.ends_with('\n'));
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn managed(directory: &Path) -> Vec<PathBuf> {
    (0..5)
        .map(|index| {
            directory.join(if index == 0 {
                "furami.log".into()
            } else {
                format!("furami.{index}.log")
            })
        })
        .filter(|path| path.exists())
        .collect()
}
fn assert_bounds_and_context(directory: &Path, limits: LogLimits) {
    let files = managed(directory);
    assert!(files.len() <= limits.files);
    let mut total = 0;
    for path in files {
        let bytes = fs::metadata(&path).unwrap().len();
        assert!(bytes <= limits.file_bytes);
        total += bytes;
        let lines = records(&path);
        assert_eq!(lines[0]["application"], "furami");
        assert_eq!(lines[0]["record_type"], "segment");
        let mut reports = BTreeMap::new();
        let text = fs::read_to_string(&path).unwrap();
        for raw in text.lines().skip(1) {
            let line: Value = serde_json::from_str(raw).unwrap();
            assert!(
                raw.len()
                    < if line["record_type"] == "report" {
                        limits.report_bytes
                    } else {
                        limits.event_bytes
                    }
            );
        }
        for line in lines.into_iter().skip(1) {
            let key = (
                line["instance_id"].as_str().unwrap().to_owned(),
                line["report_revision"].as_u64().unwrap(),
            );
            if line["record_type"] == "report" {
                reports.insert(key, line["report"].clone());
            } else {
                assert_eq!(line["schema"], 1);
                let context = reports.get(&key).expect("every event has its own instance/revision checkpoint earlier in the same segment");
                if let Some(phase) = line["fields"]["phase"].as_str() {
                    assert_eq!(
                        context["session"].get("phase").unwrap_or(&context["phase"]),
                        phase
                    );
                }
            }
        }
    }
    assert!(total <= limits.files as u64 * limits.file_bytes);
}

#[test]
fn roots_are_absolute_lossless_and_never_fall_back_to_cwd() {
    let directory = Directory::new();
    let valid = logging::resolve_location(&directory.config()).unwrap();
    assert_eq!(valid.path, directory.logs().to_str().unwrap());
    assert_eq!(valid.origin, LogOrigin::XdgStateHome);
    let home = DiagnosticsConfig {
        xdg_state_home: Some("".into()),
        home: Some(directory.0.clone().into_os_string()),
        limits: limits(),
        logging_enabled: false,
    };
    assert_eq!(
        logging::resolve_location(&home).unwrap().origin,
        LogOrigin::HomeFallback
    );
    let relative = DiagnosticsConfig {
        xdg_state_home: Some("relative".into()),
        ..home
    };
    assert_eq!(
        logging::resolve_location(&relative).unwrap_err().code,
        DiagnosticsCode::InvalidStateRoot
    );
    let missing = DiagnosticsConfig {
        xdg_state_home: None,
        home: None,
        limits: limits(),
        logging_enabled: false,
    };
    assert_eq!(
        logging::resolve_location(&missing).unwrap_err().code,
        DiagnosticsCode::MissingHome
    );
    let non_utf8 = DiagnosticsConfig {
        xdg_state_home: Some(std::ffi::OsString::from_vec(vec![b'/', 0xff])),
        ..missing
    };
    assert_eq!(
        logging::resolve_location(&non_utf8).unwrap_err().code,
        DiagnosticsCode::PathEncoding
    );
    assert!(!directory.logs().exists(), "resolution is pure");
}

#[test]
fn rotation_is_bounded_and_each_retained_event_has_its_report() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    publish(&harness.handle, report(1, "Running"));
    for index in 0..2500 {
        harness.event(index);
        harness.drain();
    }
    assert_eq!(harness.handle.health().sink_status, SinkStatus::Ready);
    assert_eq!(managed(&directory.logs()).len(), 5);
    assert_eq!(
        harness.handle.health().oversized_reports,
        0,
        "ordinary rotation exercises full reports, not emergency fallback"
    );
    for path in managed(&directory.logs()) {
        let checkpoints: Vec<_> = records(&path)
            .into_iter()
            .filter(|line| line["record_type"] == "report")
            .collect();
        assert!(!checkpoints.is_empty());
        for checkpoint in checkpoints {
            assert!(
                checkpoint["report"]["environment"]["build_inventory"]["reference_snapshot"]
                    .is_object()
            );
            assert!(checkpoint["report"]["session"].is_object());
            assert!(checkpoint["report"]["incomplete"].is_null());
        }
    }
    assert_bounds_and_context(&directory.logs(), limits());
    assert_eq!(
        fs::metadata(directory.logs()).unwrap().mode() & 0o777,
        0o700
    );
    for path in managed(&directory.logs()) {
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }
    let lock = fs::metadata(directory.logs().join(".furami-log.lock")).unwrap();
    assert_eq!(lock.len(), 0);
    assert_eq!(lock.mode() & 0o777, 0o600);
}

#[test]
fn incomplete_json_tail_is_repaired_but_foreign_and_symlink_entries_are_untouched() {
    let directory = Directory::new();
    {
        let mut harness = Harness::new(directory.config());
        publish(&harness.handle, report(1, "Running"));
        harness.event(1);
        harness.drain();
    }
    let active = directory.logs().join("furami.log");
    let before = fs::metadata(&active).unwrap().len();
    fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .unwrap()
        .write_all(b"{\"interrupted\":")
        .unwrap();
    {
        let mut harness = Harness::new(directory.config());
        publish(&harness.handle, report(1, "Recovered"));
        harness.event(2);
        harness.drain();
        assert_eq!(harness.handle.health().sink_status, SinkStatus::Ready);
    }
    assert!(!fs::read_to_string(&active).unwrap().contains("interrupted"));
    assert!(fs::metadata(&active).unwrap().len() >= before);
    assert_bounds_and_context(&directory.logs(), limits());
    let foreign = directory.logs().join("furami.4.log");
    fs::write(&foreign, b"not owned by furami\n").unwrap();
    let mut harness = Harness::new(directory.config());
    harness.event(3);
    harness.drain();
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::UnsafeEntry
    );
    assert_eq!(fs::read(&foreign).unwrap(), b"not owned by furami\n");

    let other = Directory::new();
    fs::create_dir_all(other.logs()).unwrap();
    let target = other.0.join("do-not-touch");
    fs::write(&target, b"private").unwrap();
    symlink(&target, other.logs().join("furami.log")).unwrap();
    let mut harness = Harness::new(other.config());
    harness.event(1);
    harness.drain();
    assert_eq!(harness.handle.health().sink_status, SinkStatus::Unavailable);
    assert_eq!(fs::read(&target).unwrap(), b"private");
    assert!(
        fs::symlink_metadata(other.logs().join("furami.log"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn oversized_recognized_segments_are_repaired_before_admission_and_holes_are_valid() {
    let directory = Directory::new();
    {
        let mut harness = Harness::new(directory.config());
        publish(&harness.handle, report(1, "Running"));
        harness.event(1);
        harness.drain();
    }
    let active = directory.logs().join("furami.log");
    let mut bytes = fs::read(&active).unwrap();
    let mut repeated = serde_json::to_vec(records(&active).last().unwrap()).unwrap();
    repeated.push(b'\n');
    while bytes.len() < limits().file_bytes as usize * 2 {
        bytes.extend_from_slice(&repeated);
    }
    fs::write(&active, bytes).unwrap();
    fs::rename(&active, directory.logs().join("furami.2.log")).unwrap();
    let mut harness = Harness::new(directory.config());
    publish(&harness.handle, report(1, "Restarted"));
    harness.event(2);
    harness.drain();
    assert_eq!(harness.handle.health().sink_status, SinkStatus::Ready);
    assert_bounds_and_context(&directory.logs(), limits());
}

#[test]
fn queued_events_retain_exact_reports_while_latest_report_survives_full_queue() {
    let directory = Directory::new();
    let mut config = directory.config();
    config.limits.queue_messages = 2;
    let mut harness = Harness::new(config);
    publish(&harness.handle, report(1, "Live"));
    harness.handle.event(
        EventLevel::Info,
        "apply_runtime",
        &[SafeField::token("phase", "Live")],
    );
    publish(&harness.handle, report(2, "Paused"));
    harness.event(99);
    assert!(harness.handle.health().dropped_events > 0);
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::QueueFull
    );
    harness.drain();
    let all = records(&directory.logs().join("furami.log"));
    assert!(
        all.iter()
            .any(|line| line["record_type"] == "report" && line["report_revision"] == 2)
    );
    assert_bounds_and_context(&directory.logs(), limits());
}

#[test]
fn oversize_lines_are_rejected_whole_and_emergency_report_is_explicitly_incomplete() {
    let directory = Directory::new();
    let mut config = directory.config();
    config.limits.event_bytes = 256;
    config.limits.report_bytes = 512;
    let mut harness = Harness::new(config);
    let mut huge = report(1, "Running");
    huge.session.phase = "private-user-marker".repeat(500);
    publish(&harness.handle, huge);
    let fields: Vec<_> = (0..32)
        .map(|_| SafeField::u64("apply_id", u64::MAX))
        .collect();
    harness
        .handle
        .event(EventLevel::Info, "apply_runtime", &fields);
    harness.drain();
    let health = harness.handle.health();
    assert!(health.oversized_records > 0);
    assert!(health.oversized_reports > 0);
    let text = fs::read_to_string(directory.logs().join("furami.log")).unwrap();
    assert!(!text.contains("private-user-marker"));
    assert!(
        records(&directory.logs().join("furami.log"))
            .iter()
            .any(|line| line["report"]["incomplete"] == true)
    );
}

#[test]
fn tracing_never_formats_debug_display_or_private_text_in_file_or_console() {
    use tracing_subscriber::prelude::*;
    struct Explosive;
    impl std::fmt::Debug for Explosive {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("Debug must never run")
        }
    }
    impl std::fmt::Display for Explosive {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("Display must never run")
        }
    }
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    publish(&harness.handle, report(1, "Running"));
    let subscriber =
        tracing_subscriber::registry().with(logging::SafeLayer::new(harness.handle.clone()));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(event_code = "capture_requested", width = 1920_u64, height = 1080_u64, vendor_id = 0x32ed_u64, product_id = 0x3701_u64, path = "/dev/video-private-user-marker", serial = "SERIAL-private-user-marker", topology = "TOPO-private-user-marker", label = "LABEL-private-user-marker", error = ?Explosive, displayed = %Explosive, "private-user-marker");
        tracing::warn!(unrecognized = ?Explosive, "private-user-marker");
        tracing::info!(
            event_code = "mpv_log",
            severity = "error",
            text = "private-user-marker",
            prefix = "private-user-marker"
        );
    });
    harness.drain();
    let file = fs::read_to_string(directory.logs().join("furami.log")).unwrap();
    let console = String::from_utf8(harness.console.0.lock().clone()).unwrap();
    for output in [file, console] {
        assert!(!output.contains("private-user-marker"));
        let lines: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            lines
                .iter()
                .any(|line| line["event_code"] == "fields_omitted")
        );
        assert!(
            lines.iter().any(
                |line| line["fields"]["width"] == 1920 && line["fields"]["vendor_id"] == 0x32ed
            )
        );
    }
}

#[test]
fn native_boundary_keeps_exact_ids_and_geometry_but_never_arbitrary_tokens() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    publish(&harness.handle, report(1, "Running"));
    logging::native_event(
        &harness.handle,
        "native_destroy",
        4,
        r#"{"generation":18446744073709551615,"host":4294967295,"root":17,"authorized":true,"poisoned":false,"diagnostic":"PRIVATE_NATIVE"}"#,
    );
    logging::native_event(
        &harness.handle,
        "ui_control",
        4,
        r#"{"object_name":"menuCloseCapture","x":-2,"y":3,"width":19,"height":23,"visible":true,"enabled":false}"#,
    );
    logging::native_event(
        &harness.handle,
        "ui_control",
        4,
        r#"{"object_name":"PRIVATE_ASCII","x":5,"phase":"PRIVATE_ASCII","message":"PRIVATE_NATIVE"}"#,
    );
    logging::native_event(
        &harness.handle,
        "qt_geometry",
        4,
        r#"{"root":17,"generation":18446744073709551615,"root_width":1280,"root_height":720,"video_x":1.25,"video_y":2.5,"video_width":1200.5,"video_height":680.25,"dpr":1.5}"#,
    );
    logging::native_event(
        &harness.handle,
        "native_create",
        4,
        r#"{"generation":-1,"root":17.5,"authorized":"true","object_name":"PRIVATE_ASCII"}"#,
    );
    logging::native_event(
        &harness.handle,
        "input_forward",
        4,
        r#"{"source":"PRIVATE_ASCII","event_type":6,"text_entry":true,"generation":1}"#,
    );
    logging::native_event(
        &harness.handle,
        "ui_control",
        4,
        r#"{"object_name":"renderChoice_scale_PRIVATE_ASCII","ordinal":3,"width":42}"#,
    );
    logging::native_event(
        &harness.handle,
        "ui_control",
        4,
        r#"{"object_name":"renderChoice_scale","ordinal":4,"width":43}"#,
    );
    harness.handle.event(
        EventLevel::Info,
        "apply_runtime",
        &[
            SafeField::token("phase", "Active"),
            SafeField::token("cleanup", "Complete"),
            SafeField::token("status", "PRIVATE_ASCII"),
            SafeField::u64("apply_id", u64::MAX),
        ],
    );
    harness.drain();
    for text in [
        fs::read_to_string(directory.logs().join("furami.log")).unwrap(),
        String::from_utf8(harness.console.0.lock().clone()).unwrap(),
    ] {
        assert!(!text.contains("PRIVATE_"));
        let events: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|line: &Value| line.get("event_code").is_some())
            .collect();
        let destroy = events
            .iter()
            .find(|line| line["event_code"] == "native_destroy")
            .unwrap();
        assert_eq!(destroy["fields"]["generation"].as_u64(), Some(u64::MAX));
        assert_eq!(destroy["fields"]["authorized"], true);
        let geometry = events
            .iter()
            .find(|line| line["event_code"] == "qt_geometry")
            .unwrap();
        assert_eq!(geometry["fields"]["video_width"], 1200.5);
        assert_eq!(geometry["fields"]["dpr"], 1.5);
        assert!(
            events
                .iter()
                .any(|line| line["fields"]["object_name"] == "menuCloseCapture"
                    && line["fields"]["x"] == -2
                    && line["fields"]["enabled"] == false)
        );
        let invalid_ids = events
            .iter()
            .find(|line| line["event_code"] == "native_create")
            .unwrap();
        assert_eq!(invalid_ids["fields"], serde_json::json!({}));
        let forwarding = events
            .iter()
            .find(|line| line["event_code"] == "input_forward")
            .unwrap();
        assert!(forwarding["fields"].get("source").is_none());
        assert_eq!(forwarding["fields"]["event_type"], 6);
        assert!(
            events
                .iter()
                .any(|line| line["fields"]["object_name"] == "renderChoice_scale"
                    && line["fields"]["ordinal"] == 4
                    && line["fields"]["width"] == 43)
        );
        let apply = events
            .iter()
            .find(|line| line["event_code"] == "apply_runtime")
            .unwrap();
        assert_eq!(apply["fields"]["cleanup"], "Complete");
        assert_eq!(apply["fields"]["apply_id"].as_u64(), Some(u64::MAX));
        assert!(apply["fields"].get("status").is_none());
    }
}

#[test]
fn partial_write_rolls_back_and_rotation_fault_disables_only_storage() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    publish(&harness.handle, report(1, "Running"));
    harness.event(1);
    harness.drain();
    let active = directory.logs().join("furami.log");
    let before = fs::read(&active).unwrap();
    harness.worker.set_failpoint(Failpoint::PartialWrite(7));
    harness.event(2);
    harness.drain();
    assert_eq!(fs::read(&active).unwrap(), before);
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::WriteFailed
    );
    assert_eq!(harness.handle.health().sink_status, SinkStatus::Unavailable);
    assert!(!harness.console.0.lock().is_empty());

    let other = Directory::new();
    let mut config = other.config();
    config.limits.file_bytes = 4096;
    config.limits.report_bytes = 2048;
    let mut harness = Harness::new(config);
    publish(&harness.handle, report(1, "Running"));
    harness.drain();
    harness.worker.set_failpoint(Failpoint::Rotation);
    for index in 0..50 {
        harness.event(index);
        harness.drain();
    }
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::RotationFailed
    );
    assert_eq!(harness.handle.health().sink_status, SinkStatus::Unavailable);
}

#[test]
fn console_failure_keeps_file_sink_healthy_and_folder_success_does_not_clear_it() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    harness.console.1.store(true, Ordering::Relaxed);
    publish(&harness.handle, report(1, "Running"));
    harness.event(1);
    harness.drain();
    assert_eq!(harness.handle.health().sink_status, SinkStatus::Ready);
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::ConsoleWriteFailed
    );
    harness.handle.prepare_log_folder(FolderRequestId(1));
    harness.drain();
    let folder = harness.handle.take_folder_result().unwrap();
    assert_eq!(folder.result.unwrap(), directory.logs().to_str().unwrap());
    assert!(harness.handle.take_folder_result().is_none());
    assert!(harness.handle.claim_log_folder_dispatch(FolderRequestId(1)));
    harness
        .handle
        .acknowledge_log_folder(FolderRequestId(1), true);
    harness.handle.finish_log_folder(FolderRequestId(1), false);
    assert_eq!(
        harness.handle.health().folder.state,
        FolderState::OpenRequested
    );
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::ConsoleWriteFailed
    );
}

#[test]
fn folder_ids_are_correlated_and_old_or_duplicate_acks_cannot_erase_new_failure() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    harness.handle.prepare_log_folder(FolderRequestId(1));
    harness.drain();
    assert!(harness.handle.take_folder_result().unwrap().result.is_ok());
    assert!(harness.handle.claim_log_folder_dispatch(FolderRequestId(1)));
    harness
        .handle
        .acknowledge_log_folder(FolderRequestId(1), false);
    harness.handle.finish_log_folder(FolderRequestId(1), false);
    harness.handle.finish_log_folder(FolderRequestId(1), true);
    assert_eq!(
        harness.handle.health().folder.error.unwrap().code,
        DiagnosticsCode::OpenRejected
    );
    harness.handle.prepare_log_folder(FolderRequestId(2));
    harness.drain();
    harness.handle.take_folder_result().unwrap();
    assert!(harness.handle.claim_log_folder_dispatch(FolderRequestId(2)));
    harness
        .handle
        .acknowledge_log_folder(FolderRequestId(1), true);
    assert_eq!(harness.handle.health().folder.state, FolderState::Preparing);
    harness
        .handle
        .acknowledge_log_folder(FolderRequestId(2), false);
    harness
        .handle
        .acknowledge_log_folder(FolderRequestId(2), true);
    assert_eq!(harness.handle.health().folder.state, FolderState::Failed);
    assert_eq!(
        harness.handle.health().folder.error.unwrap().code,
        DiagnosticsCode::OpenRejected
    );
    assert!(FolderRequestId(u64::MAX).next().is_none());
    assert!(ReportRevision(u64::MAX).next().is_none());
}

#[test]
fn folder_terminal_requires_accepted_start_and_consumes_the_outcome_once() {
    for failed in [false, true] {
        let directory = Directory::new();
        let mut harness = Harness::new(directory.config());
        let request = FolderRequestId(1);
        harness.handle.prepare_log_folder(request);
        harness.drain();
        harness.handle.take_folder_result().unwrap();
        harness.handle.finish_log_folder(request, true);
        assert_eq!(harness.handle.health().folder.state, FolderState::Preparing);
        assert!(harness.handle.claim_log_folder_dispatch(request));
        harness.handle.finish_log_folder(request, true);
        assert_eq!(harness.handle.health().folder.state, FolderState::Preparing);
        harness.handle.acknowledge_log_folder(request, true);
        harness.handle.finish_log_folder(FolderRequestId(2), true);
        assert_eq!(
            harness.handle.health().folder.state,
            FolderState::OpenRequested
        );
        harness.handle.finish_log_folder(request, failed);
        let completed = harness.handle.health().folder;
        assert_eq!(
            completed.state,
            if failed {
                FolderState::Failed
            } else {
                FolderState::OpenRequested
            }
        );
        assert_eq!(
            completed.error.map(|error| (error.operation, error.code)),
            failed.then_some((
                DiagnosticsOperation::OpenFolder,
                DiagnosticsCode::LauncherFailed
            ))
        );
        harness.handle.acknowledge_log_folder(request, true);
        harness.handle.acknowledge_log_folder(request, false);
        harness.handle.finish_log_folder(request, !failed);
        harness.handle.finish_log_folder(request, failed);
        assert_eq!(harness.handle.health().folder, completed);
    }
}

#[test]
fn folder_terminal_cannot_change_a_newer_preparation_or_launch_outcome() {
    for newer_accepted in [false, true] {
        let directory = Directory::new();
        let mut harness = Harness::new(directory.config());
        let old = FolderRequestId(1);
        let latest = FolderRequestId(2);
        harness.handle.prepare_log_folder(old);
        harness.drain();
        harness.handle.take_folder_result().unwrap();
        assert!(harness.handle.claim_log_folder_dispatch(old));
        harness.handle.acknowledge_log_folder(old, true);
        harness.handle.prepare_log_folder(latest);
        harness.handle.finish_log_folder(old, true);
        harness.handle.finish_log_folder(old, false);
        assert_eq!(harness.handle.health().folder.state, FolderState::Preparing);
        assert_eq!(harness.handle.health().folder.request, Some(latest));
        harness.drain();
        harness.handle.take_folder_result().unwrap();
        assert!(harness.handle.claim_log_folder_dispatch(latest));
        harness
            .handle
            .acknowledge_log_folder(latest, newer_accepted);
        let started = harness.handle.health().folder;
        harness.handle.finish_log_folder(old, false);
        harness.handle.finish_log_folder(old, true);
        assert_eq!(harness.handle.health().folder, started);
        harness.handle.finish_log_folder(latest, true);
        let failure = harness.handle.health().folder;
        assert_eq!(failure.state, FolderState::Failed);
        assert_eq!(
            failure.error.unwrap().code,
            if newer_accepted {
                DiagnosticsCode::LauncherFailed
            } else {
                DiagnosticsCode::OpenRejected
            }
        );
        harness.handle.finish_log_folder(old, false);
        harness.handle.finish_log_folder(old, true);
        assert_eq!(harness.handle.health().folder, failure);
    }
}

#[test]
fn folder_terminal_is_cancelled_by_disable_including_after_reenable() {
    for finish_while_disabled in [false, true] {
        let directory = Directory::new();
        let mut harness = Harness::new(directory.config());
        let old = FolderRequestId(1);
        let latest = FolderRequestId(2);
        harness.handle.prepare_log_folder(old);
        harness.drain();
        harness.handle.take_folder_result().unwrap();
        assert!(harness.handle.claim_log_folder_dispatch(old));
        harness.handle.acknowledge_log_folder(old, true);
        let accepted = harness.handle.health().folder;
        harness.handle.set_logging_enabled(false).unwrap();
        if finish_while_disabled {
            harness.handle.finish_log_folder(old, true);
        }
        assert_eq!(harness.handle.health().folder, accepted);
        harness.handle.set_logging_enabled(true).unwrap();
        harness.handle.finish_log_folder(old, true);
        assert_eq!(harness.handle.health().folder, accepted);
        harness.handle.prepare_log_folder(latest);
        harness.drain();
        harness.handle.take_folder_result().unwrap();
        assert!(harness.handle.claim_log_folder_dispatch(latest));
        harness.handle.acknowledge_log_folder(latest, true);
        harness.handle.finish_log_folder(old, true);
        assert_eq!(
            harness.handle.health().folder.state,
            FolderState::OpenRequested
        );
        harness.handle.finish_log_folder(latest, true);
        assert_eq!(
            harness.handle.health().folder.error.unwrap().code,
            DiagnosticsCode::LauncherFailed
        );
    }
}

#[test]
fn folder_counter_exhaustion_invalidates_an_already_started_launcher() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    let request = FolderRequestId(u64::MAX);
    harness.handle.prepare_log_folder(request);
    harness.drain();
    harness.handle.take_folder_result().unwrap();
    assert!(harness.handle.claim_log_folder_dispatch(request));
    harness.handle.acknowledge_log_folder(request, true);
    harness.handle.reject_folder_counter_exhausted();
    harness.handle.finish_log_folder(request, false);
    harness.handle.finish_log_folder(request, true);
    assert_eq!(harness.handle.health().folder.state, FolderState::Failed);
    assert_eq!(
        harness.handle.health().folder.error.unwrap().code,
        DiagnosticsCode::CounterExhausted
    );
}

#[test]
fn folder_terminal_failure_preserves_sink_health_and_loss_accounting() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    harness.console.1.store(true, Ordering::Relaxed);
    publish(&harness.handle, report(1, "Running"));
    harness.event(1);
    harness.drain();
    let request = FolderRequestId(1);
    harness.handle.prepare_log_folder(request);
    harness.drain();
    harness.handle.take_folder_result().unwrap();
    assert!(harness.handle.claim_log_folder_dispatch(request));
    harness.handle.acknowledge_log_folder(request, true);
    for _ in 0..=limits().queue_messages {
        harness.event(2);
    }
    let before = harness.handle.health();
    assert!(before.last_error.is_some());
    assert!(before.dropped_events > 0);
    harness.handle.finish_log_folder(request, true);
    let failed = harness.handle.health();
    assert_eq!(
        failed.folder.error.unwrap().code,
        DiagnosticsCode::LauncherFailed
    );
    assert_eq!(failed.sink_status, before.sink_status);
    assert_eq!(failed.sink_error, before.sink_error);
    assert_eq!(failed.last_error, before.last_error);
    assert_eq!(failed.dropped_events, before.dropped_events);
    assert_eq!(failed.oversized_records, before.oversized_records);
    assert_eq!(failed.oversized_reports, before.oversized_reports);
}

#[test]
fn directory_permission_lock_and_worker_failures_are_typed_not_native_text() {
    let directory = Directory::new();
    let obstructed = directory.0.join("root-is-a-file");
    fs::write(&obstructed, b"untouched").unwrap();
    let mut config = directory.config();
    config.xdg_state_home = Some(obstructed.into_os_string());
    let mut harness = Harness::new(config);
    harness.event(1);
    harness.drain();
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::DirectoryAccess
    );
    assert_eq!(harness.handle.health().sink_status, SinkStatus::Unavailable);

    let mut harness = Harness::new(directory.config());
    harness.worker.set_failpoint(Failpoint::LockUnsupported);
    harness.event(1);
    harness.drain();
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::LockUnsupported
    );
    drop(harness.worker);
    harness.handle.event(
        EventLevel::Info,
        "apply_runtime",
        &[SafeField::u64("apply_id", 2)],
    );
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::WorkerStopped
    );
}

#[test]
fn nofollow_parent_walk_refuses_symlink_substitution_and_preserves_foreign_tree() {
    let directory = Directory::new();
    let target = Directory::new();
    symlink(&target.0, directory.0.join("furami")).unwrap();
    let mut harness = Harness::new(directory.config());
    harness.event(1);
    harness.drain();
    assert_eq!(harness.handle.health().sink_status, SinkStatus::Unavailable);
    assert!(!target.0.join("logs").exists());
}

#[test]
fn real_lock_contention_is_worker_only_and_never_writes_unlocked() {
    let directory = Directory::new();
    {
        let mut harness = Harness::new(directory.config());
        harness.drain();
    }
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.logs().join(".furami-log.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let before = fs::read(directory.logs().join("furami.log")).unwrap();
    let mut harness = Harness::new(directory.config());
    harness.worker.set_lock_timeout(Duration::ZERO);
    harness.event(1);
    harness.drain();
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::LockContended
    );
    assert_eq!(
        fs::read(directory.logs().join("furami.log")).unwrap(),
        before
    );
}

#[test]
fn os_release_parser_is_bounded_and_does_not_dump_unrelated_environment() {
    let observed = logging::parse_os_release(
        b"NAME=private-user-marker\nID=example\nVERSION_ID=\"44\"\nHOME=/private-user-marker\n",
    );
    let text = serde_json::to_string(&observed).unwrap();
    assert!(text.contains("example"));
    assert!(text.contains("44"));
    assert!(!text.contains("private-user-marker"));
    assert!(matches!(
        logging::parse_os_release(&vec![b'a'; 16_385]),
        Observation::Unavailable { .. }
    ));
}

#[test]
fn invalid_log_filter_is_visible_and_missing_filter_is_normal_info() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    let before = harness.handle.health().last_error;
    let _ = logging::log_filter(None, &harness.handle);
    assert_eq!(harness.handle.health().last_error, before);
    let _ = logging::log_filter(Some("[invalid".into()), &harness.handle);
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::InvalidLogFilter
    );
    harness.drain();
}

#[test]
fn shutdown_closes_admission_without_joining_active_worker() {
    let directory = Directory::new();
    let runtime = DiagnosticsRuntime::start(directory.config());
    let handle = runtime.handle();
    runtime.begin_shutdown();
    handle.event(EventLevel::Info, "apply_runtime", &[]);
    assert_eq!(
        handle.health().last_error.unwrap().code,
        DiagnosticsCode::WorkerStopped
    );
    runtime.finish_shutdown(Duration::from_millis(250));
}

// A real second process, using the same unit-test executable and the actual
// storage worker. Parent/child handshakes provide ordering, not timing sleeps.
#[test]
fn multiprocess_child() {
    let Some(root) = std::env::var_os("FURAMI_DIAGNOSTICS_TEST_ROOT") else {
        return;
    };
    let phase = std::env::var("FURAMI_DIAGNOSTICS_TEST_PHASE").unwrap();
    let config = DiagnosticsConfig {
        xdg_state_home: Some(root),
        home: None,
        limits: limits(),
        logging_enabled: true,
    };
    let mut harness = Harness::new(config);
    publish(&harness.handle, report(1, &phase));
    harness.drain();
    println!("FURAMI_READY");
    io::stdout().flush().unwrap();
    for line in io::stdin().lock().lines() {
        if line.unwrap() == "stop" {
            break;
        }
        harness.handle.event(
            EventLevel::Info,
            "apply_runtime",
            &[SafeField::token("phase", &phase)],
        );
        harness.drain();
        assert_eq!(harness.handle.health().sink_status, SinkStatus::Ready);
        println!("FURAMI_WROTE");
        io::stdout().flush().unwrap();
    }
}

#[test]
fn two_real_processes_share_retention_without_cross_wiring_revision_one() {
    struct Child {
        process: std::process::Child,
        output: BufReader<std::process::ChildStdout>,
    }
    impl Child {
        fn wait_for(&mut self, marker: &str) {
            loop {
                let mut line = String::new();
                assert_ne!(
                    self.output.read_line(&mut line).unwrap(),
                    0,
                    "child exited before {marker}"
                );
                if line.trim() == marker {
                    break;
                }
            }
        }
        fn command(&mut self, command: &str) {
            writeln!(self.process.stdin.as_mut().unwrap(), "{command}").unwrap();
            self.process.stdin.as_mut().unwrap().flush().unwrap();
        }
    }
    impl Drop for Child {
        fn drop(&mut self) {
            if self.process.try_wait().ok().flatten().is_none() {
                let _ = self.process.kill();
                let _ = self.process.wait();
            }
        }
    }
    fn spawn(directory: &Directory, phase: &str) -> Child {
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::logging_tests::multiprocess_child",
                "--nocapture",
            ])
            .env("FURAMI_DIAGNOSTICS_TEST_ROOT", &directory.0)
            .env("FURAMI_DIAGNOSTICS_TEST_PHASE", phase)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = BufReader::new(process.stdout.take().unwrap());
        let mut child = Child { process, output };
        child.wait_for("FURAMI_READY");
        child
    }
    let directory = Directory::new();
    let mut first = spawn(&directory, "Live");
    let mut second = spawn(&directory, "Paused");
    for _ in 0..1250 {
        first.command("write");
        first.wait_for("FURAMI_WROTE");
        second.command("write");
        second.wait_for("FURAMI_WROTE");
    }
    first.command("stop");
    second.command("stop");
    assert!(first.process.wait().unwrap().success());
    assert!(second.process.wait().unwrap().success());
    assert_bounds_and_context(&directory.logs(), limits());
    assert_eq!(managed(&directory.logs()).len(), limits().files);
    let mut identities = std::collections::BTreeSet::new();
    for path in managed(&directory.logs()) {
        for line in records(&path).into_iter().skip(1) {
            if line["event_code"].is_string() {
                identities.insert(line["instance_id"].as_str().unwrap().to_owned());
            }
        }
    }
    assert_eq!(identities.len(), 2);
}

#[test]
fn superseding_latest_slot_never_emits_bootstrap_or_older_overflow_report() {
    let directory = Directory::new();
    let mut config = directory.config();
    config.limits.queue_messages = 2;
    let mut harness = Harness::new(config);
    publish(&harness.handle, report(1, "Live"));
    harness.event(1);
    publish(&harness.handle, report(2, "Paused"));
    assert!(harness.worker.process_one_for_test());
    publish(&harness.handle, report(3, "Live"));
    harness.drain();
    let revisions: Vec<_> = records(&directory.logs().join("furami.log"))
        .into_iter()
        .filter(|line| line["record_type"] == "report")
        .map(|line| line["report_revision"].as_u64().unwrap())
        .collect();
    assert_eq!(revisions, vec![1, 3]);
}

#[test]
fn checkpoint_survives_interleaving_but_is_reestablished_after_same_inode_truncate_regrow() {
    let directory = Directory::new();
    let mut first = Harness::new(directory.config());
    publish(&first.handle, report(1, "Live"));
    first.handle.event(
        EventLevel::Info,
        "apply_runtime",
        &[SafeField::token("phase", "Live")],
    );
    first.drain();
    let path = directory.logs().join("furami.log");
    let before = fs::metadata(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    let header_length = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(header_length as u64)
        .unwrap();
    let mut second = Harness::new(directory.config());
    publish(&second.handle, report(1, "Paused"));
    for _ in 0..8 {
        second.handle.event(
            EventLevel::Info,
            "apply_runtime",
            &[SafeField::token("phase", "Paused")],
        );
        second.drain();
    }
    let regrown = fs::metadata(&path).unwrap();
    assert_eq!(regrown.ino(), before.ino());
    assert!(regrown.len() > before.len());
    first.handle.event(
        EventLevel::Info,
        "apply_runtime",
        &[SafeField::token("phase", "Live")],
    );
    first.drain();
    assert_bounds_and_context(&directory.logs(), limits());
}

#[test]
fn failed_emergency_rejects_new_events_instead_of_reusing_previous_session_context() {
    let directory = Directory::new();
    let mut config = directory.config();
    config.limits.report_bytes = 480;
    let mut harness = Harness::new(config);
    publish(&harness.handle, report(1, "Live"));
    harness.event(1);
    harness.drain();
    let mut rejected = report(2, "Paused");
    rejected.session.identity = SessionIdentity {
        operation: Some(u64::MAX),
        attempt: Some(u64::MAX),
        generation: Some(u64::MAX),
    };
    rejected.session.startup_failure = Some(StartupFailure {
        code: "InvalidDisplayEnvironment",
        stage: "NativeStartup",
        requested: Some(StartupTuple {
            format_policy: "Manual",
            fourcc: Some(u32::MAX),
            width: u32::MAX,
            height: u32::MAX,
            rate_numerator: u32::MAX,
            rate_denominator: u32::MAX,
        }),
    });
    publish(&harness.handle, rejected);
    let before = harness.handle.health().dropped_events;
    harness.event(2);
    assert!(harness.handle.health().dropped_events > before);
    harness.drain();
    assert!(
        !records(&directory.logs().join("furami.log"))
            .iter()
            .any(|line| line["fields"]["apply_id"] == 2)
    );
    assert!(harness.handle.health().sink_error.is_none());
    publish(&harness.handle, report(3, "Paused"));
    harness.event(3);
    harness.drain();
    assert!(
        records(&directory.logs().join("furami.log"))
            .iter()
            .any(|line| line["fields"]["apply_id"] == 3 && line["report_revision"] == 3)
    );
}

#[test]
fn minimal_bootstrap_limit_never_emits_partial_json_or_permanently_disables_storage() {
    let directory = Directory::new();
    let mut config = directory.config();
    config.limits.report_bytes = 256;
    let mut harness = Harness::new(config);
    harness.drain();
    assert!(harness.handle.health().oversized_reports > 0);
    assert!(harness.handle.health().sink_error.is_none());
    for path in managed(&directory.logs()) {
        let _ = records(&path);
    }
    let console = harness.console.0.lock();
    for line in console
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let _: Value = serde_json::from_slice(line).unwrap();
    }
}

#[test]
fn sticky_sink_error_survives_later_console_queue_and_folder_results() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    publish(&harness.handle, report(1, "Live"));
    harness.drain();
    harness.worker.set_failpoint(Failpoint::PartialWrite(3));
    harness.event(1);
    harness.drain();
    let storage_error = harness.handle.health().sink_error.unwrap();
    assert_eq!(storage_error.code, DiagnosticsCode::WriteFailed);
    harness.console.1.store(true, Ordering::Relaxed);
    harness.event(2);
    harness.drain();
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::ConsoleWriteFailed
    );
    for index in 0..100 {
        harness.event(index);
    }
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::QueueFull
    );
    assert_eq!(harness.handle.health().sink_error, Some(storage_error));
    harness.handle.prepare_log_folder(FolderRequestId(1));
    harness.drain();
    assert!(harness.handle.take_folder_result().unwrap().result.is_ok());
    assert!(harness.handle.claim_log_folder_dispatch(FolderRequestId(1)));
    harness
        .handle
        .acknowledge_log_folder(FolderRequestId(1), true);
    assert_eq!(harness.handle.health().sink_error, Some(storage_error));
}

#[test]
fn counter_exhaustion_is_visible_and_destroys_pending_folder_dispatch_and_ack() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    harness.handle.prepare_log_folder(FolderRequestId(u64::MAX));
    harness.drain();
    assert!(harness.handle.take_folder_result().unwrap().result.is_ok());
    assert!(
        harness
            .handle
            .claim_log_folder_dispatch(FolderRequestId(u64::MAX))
    );
    harness.handle.reject_folder_counter_exhausted();
    harness
        .handle
        .acknowledge_log_folder(FolderRequestId(u64::MAX), true);
    assert!(harness.handle.take_folder_result().is_none());
    assert_eq!(harness.handle.health().folder.state, FolderState::Failed);
    assert_eq!(
        harness.handle.health().folder.error.unwrap().code,
        DiagnosticsCode::CounterExhausted
    );
    harness.handle.counter_exhausted();
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::CounterExhausted
    );
}

#[test]
fn native_geometry_is_finite_and_never_accepts_free_text_commands_or_object_names() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    logging::native_event(
        &harness.handle,
        "ui_control",
        4,
        r#"{"object_name":"menuOpenLogs","x":1,"y":2,"width":120,"height":24,"text":"private-user-marker","command":"private-user-marker","label":"private-user-marker"}"#,
    );
    logging::native_event(
        &harness.handle,
        "qt_geometry",
        4,
        r#"{"object_name":"private-user-marker","x":3,"message":"private-user-marker"}"#,
    );
    logging::native_event(
        &harness.handle,
        "private-user-marker",
        4,
        r#"{"message":"private-user-marker"}"#,
    );
    harness.drain();
    let text = fs::read_to_string(directory.logs().join("furami.log")).unwrap();
    let console = String::from_utf8(harness.console.0.lock().clone()).unwrap();
    for output in [text, console] {
        assert!(!output.contains("private-user-marker"));
        assert!(output.contains("menuOpenLogs"));
        for line in output.lines() {
            let _: Value = serde_json::from_str(line).unwrap();
        }
    }
}

#[test]
fn folder_terminal_native_events_keep_only_the_allowlisted_outcome() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    logging::native_event(
        &harness.handle,
        "log_folder_finished",
        1,
        r#"{"request_id":1,"failed":true,"exit_code":7,"path":"private-launcher-marker","stderr":"private-launcher-marker","errorString":"private-launcher-marker"}"#,
    );
    logging::native_event(
        &harness.handle,
        "log_folder_finished",
        1,
        r#"{"request_id":2,"failed":true}"#,
    );
    logging::native_event(
        &harness.handle,
        "log_folder_finished",
        4,
        r#"{"request_id":3,"failed":false,"exit_code":0}"#,
    );
    harness.drain();
    let file = records(&directory.logs().join("furami.log"));
    let console: Vec<Value> = String::from_utf8(harness.console.0.lock().clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for records in [file, console] {
        let outcomes: Vec<_> = records
            .iter()
            .filter(|record| record["event_code"] == "log_folder_finished")
            .map(|record| &record["fields"])
            .collect();
        assert_eq!(
            outcomes,
            [
                &serde_json::json!({"request_id":1,"failed":true,"exit_code":7}),
                &serde_json::json!({"request_id":2,"failed":true}),
                &serde_json::json!({"request_id":3,"failed":false,"exit_code":0}),
            ]
        );
    }
}

#[test]
fn bounded_worker_pass_services_folder_while_console_refills_the_event_queue() {
    struct Refill {
        handle: Arc<Mutex<Option<DiagnosticsHandle>>>,
        remaining: usize,
    }
    impl Write for Refill {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining != 0 {
                self.remaining -= 1;
                if let Some(handle) = self.handle.lock().as_ref() {
                    handle.event(
                        EventLevel::Info,
                        "apply_runtime",
                        &[SafeField::u64("apply_id", 42)],
                    );
                }
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let directory = Directory::new();
    let mut config = directory.config();
    config.limits.queue_messages = 2;
    let slot = Arc::new(Mutex::new(None));
    let (handle, mut worker) = logging::test_pair(
        config,
        Box::new(Refill {
            handle: Arc::clone(&slot),
            remaining: 100,
        }),
    );
    *slot.lock() = Some(handle.clone());
    publish(&handle, report(1, "Live"));
    handle.event(EventLevel::Info, "apply_runtime", &[]);
    handle.prepare_log_folder(FolderRequestId(1));
    assert!(
        worker.drain_available(),
        "the bounded pass ended while work remains"
    );
    assert!(
        handle.take_folder_result().unwrap().result.is_ok(),
        "folder work is not starved by a continuously refilled event queue"
    );
    *slot.lock() = None;
}

#[test]
fn blocked_console_never_blocks_producers_or_makes_shutdown_join_a_live_worker() {
    struct BlockingConsole {
        entered: Option<std::sync::mpsc::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Write for BlockingConsole {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let directory = Directory::new();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (runtime, finished) = logging::test_runtime(
        directory.config(),
        Box::new(BlockingConsole {
            entered: Some(entered_tx),
            release: release_rx,
        }),
    );
    entered_rx.recv().unwrap();
    let handle = runtime.handle();
    publish(&handle, report(1, "Live"));
    for index in 0..100 {
        handle.event(
            EventLevel::Info,
            "apply_runtime",
            &[SafeField::u64("apply_id", index)],
        );
    }
    assert!(handle.health().dropped_events > 0);
    runtime.finish_shutdown(Duration::ZERO);
    assert!(
        !runtime.is_finished(),
        "worker is still held by an explicit barrier"
    );
    assert_eq!(
        handle.health().last_error.unwrap().code,
        DiagnosticsCode::ShutdownIncomplete
    );
    release_tx.send(()).unwrap();
    finished.recv().unwrap();
    runtime.finish_shutdown(Duration::from_millis(250));
}

#[test]
fn rollback_and_directory_faults_retain_only_typed_evidence() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    harness.drain();
    harness.worker.set_failpoint(Failpoint::Rollback);
    harness.event(1);
    harness.drain();
    let health = harness.handle.health();
    assert_eq!(
        health.sink_error.unwrap().code,
        DiagnosticsCode::WriteFailed
    );
    assert_eq!(
        health.rollback_error.unwrap().operation,
        DiagnosticsOperation::Rollback
    );
    let other = Directory::new();
    let mut harness = Harness::new(other.config());
    harness.worker.set_failpoint(Failpoint::DirectoryAccess);
    harness.drain();
    assert_eq!(
        harness.handle.health().sink_error.unwrap().io_kind,
        Some(IoKind::PermissionDenied)
    );
}

#[test]
fn folder_resolution_failure_keeps_its_cause_after_later_queue_fault() {
    for (root, expected) in [
        (Some("relative".into()), DiagnosticsCode::InvalidStateRoot),
        (None, DiagnosticsCode::MissingHome),
    ] {
        let config = DiagnosticsConfig {
            xdg_state_home: root,
            home: None,
            limits: LogLimits {
                queue_messages: 1,
                ..limits()
            },
            logging_enabled: true,
        };
        let mut harness = Harness::new(config);
        harness.event(1);
        harness.event(2);
        assert_eq!(
            harness.handle.health().last_error.unwrap().code,
            DiagnosticsCode::QueueFull
        );
        harness.handle.prepare_log_folder(FolderRequestId(1));
        harness.drain();
        assert_eq!(
            harness
                .handle
                .take_folder_result()
                .unwrap()
                .result
                .unwrap_err()
                .code,
            expected
        );
        assert_eq!(harness.handle.health().folder.error.unwrap().code, expected);
    }
}

#[test]
fn allocated_keys_are_unique_across_bootstrap_initial_runtime_and_late_failure() {
    let directory = Directory::new();
    let mut config = directory.config();
    config.limits = LogLimits::default();
    let mut harness = Harness::new(config);
    harness.drain();
    publish(&harness.handle, report(1, "Live"));
    harness.event(1);
    harness.drain();
    let runtime_handle = harness.handle.clone();
    for (revision, phase) in [(2, "Paused"), (3, "Live"), (4, "Paused")] {
        publish(&runtime_handle, report(revision, phase));
        harness.event(revision);
        harness.drain();
    }
    let mut failure = report(5, "StartupFailed");
    failure.session.identity = SessionIdentity {
        operation: Some(91),
        attempt: Some(92),
        generation: Some(93),
    };
    failure.session.startup_failure = Some(StartupFailure {
        code: "NativeLaunch",
        stage: "NativeStartup",
        requested: Some(StartupTuple {
            format_policy: "Manual",
            fourcc: Some(0x3231564e),
            width: 1920,
            height: 1080,
            rate_numerator: 60000,
            rate_denominator: 1001,
        }),
    });
    publish(&harness.handle, failure);
    harness.handle.event(
        EventLevel::Error,
        "startup_failed",
        &[SafeField::u64("apply_id", 91)],
    );
    harness.drain();
    let lines = records(&directory.logs().join("furami.log"));
    let revisions: Vec<_> = lines
        .iter()
        .filter(|line| line["record_type"] == "report")
        .map(|line| line["report_revision"].as_u64().unwrap())
        .collect();
    assert_eq!(revisions, vec![0, 1, 2, 3, 4, 5]);
    let mut contexts = BTreeMap::new();
    for line in lines {
        if line["record_type"] == "report" {
            let key = (
                line["instance_id"].as_str().unwrap().to_owned(),
                line["report_revision"].as_u64().unwrap(),
            );
            if let Some(inner_revision) = line["report"].get("revision") {
                assert_eq!(inner_revision, &line["report_revision"]);
            }
            assert!(
                contexts.insert(key, line["report"].clone()).is_none(),
                "one immutable report per allocated key"
            );
        }
    }
    assert_bounds_and_context(&directory.logs(), LogLimits::default());
}

#[test]
fn unreserved_reused_and_stale_report_ids_cannot_replace_the_current_context() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    harness.drain();
    harness.handle.publish_report(report(1, "Paused"));
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::InvalidReportRevision
    );
    publish(&harness.handle, report(1, "Live"));
    harness.drain();
    harness.handle.publish_report(report(1, "Paused"));
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::InvalidReportRevision
    );
    assert_eq!(
        harness.handle.reserve_report_revision(),
        Some(ReportRevision(2))
    );
    assert_eq!(
        harness.handle.clone().reserve_report_revision(),
        Some(ReportRevision(3))
    );
    harness.handle.publish_report(report(3, "Live"));
    harness.handle.publish_report(report(2, "Paused"));
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::InvalidReportRevision
    );
    harness.event(99);
    harness.drain();
    let lines = records(&directory.logs().join("furami.log"));
    assert!(
        !lines
            .iter()
            .any(|line| line["record_type"] == "report" && line["report_revision"] == 2)
    );
    assert!(
        lines
            .iter()
            .any(|line| line["fields"]["apply_id"] == 99 && line["report_revision"] == 3)
    );
    assert_bounds_and_context(&directory.logs(), limits());
}

#[test]
fn shared_revision_exhaustion_refuses_new_contexts_and_events_but_not_folder_work() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    publish(&harness.handle, report(1, "Live"));
    harness.event(1);
    harness.drain();
    harness.handle.exhaust_report_revisions_for_test();
    assert_eq!(
        harness.handle.reserve_report_revision(),
        Some(ReportRevision(u64::MAX))
    );
    harness.handle.publish_report(report(u64::MAX, "Paused"));
    harness.event(2);
    harness.drain();
    assert_eq!(harness.handle.reserve_report_revision(), None);
    assert_eq!(harness.handle.clone().reserve_report_revision(), None);
    let before = harness.handle.health().dropped_events;
    harness.event(3);
    assert_eq!(harness.handle.health().dropped_events, before + 1);
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::CounterExhausted
    );
    harness
        .handle
        .publish_report(report(u64::MAX, "StartupFailed"));
    harness.handle.prepare_log_folder(FolderRequestId(1));
    harness.drain();
    assert!(harness.handle.take_folder_result().unwrap().result.is_ok());
    assert!(harness.handle.claim_log_folder_dispatch(FolderRequestId(1)));
    harness
        .handle
        .acknowledge_log_folder(FolderRequestId(1), true);
    assert_eq!(
        harness.handle.health().folder.state,
        FolderState::OpenRequested
    );
    assert_eq!(
        harness.handle.health().last_error.unwrap().code,
        DiagnosticsCode::CounterExhausted
    );
    let lines = records(&directory.logs().join("furami.log"));
    assert!(lines.iter().any(|line| line["fields"]["apply_id"] == 2
        && line["report_revision"].as_u64() == Some(u64::MAX)));
    assert!(!lines.iter().any(|line| line["fields"]["apply_id"] == 3));
    assert_bounds_and_context(&directory.logs(), limits());
}

#[test]
fn cloned_handles_allocate_distinct_revisions_across_threads() {
    let directory = Directory::new();
    let harness = Harness::new(directory.config());
    let revisions = std::thread::scope(|scope| {
        let mut threads = Vec::new();
        for _ in 0..4 {
            let handle = harness.handle.clone();
            threads.push(scope.spawn(move || {
                (0..32)
                    .map(|_| handle.reserve_report_revision().unwrap())
                    .collect::<Vec<_>>()
            }));
        }
        threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
    });
    assert_eq!(revisions.len(), 128);
    assert_eq!(revisions.first(), Some(&ReportRevision(1)));
    assert_eq!(revisions.last(), Some(&ReportRevision(128)));
}

#[test]
fn default_and_explicit_off_bootstrap_leave_absent_log_tree_and_console_empty() {
    assert!(!DiagnosticsConfig::default().logging_enabled);
    for use_default in [true, false] {
        let directory = Directory::new();
        let config = if use_default {
            DiagnosticsConfig {
                xdg_state_home: Some(directory.0.clone().into_os_string()),
                home: None,
                ..DiagnosticsConfig::default()
            }
        } else {
            DiagnosticsConfig {
                logging_enabled: false,
                ..directory.config()
            }
        };
        let mut harness = Harness::new(config);
        publish(&harness.handle, report(1, "Paused"));
        harness.event(1);
        harness.handle.prepare_log_folder(FolderRequestId(1));
        harness.drain();
        assert!(!directory.logs().exists());
        assert!(!directory.0.join("furami").exists());
        assert!(harness.console.0.lock().is_empty());
        let health = harness.handle.health();
        assert!(!health.logging_enabled);
        assert!(!health.has_problem());
        assert_eq!(health.message_fr(), "");
        assert_eq!(health.dropped_events, 0);
        assert_eq!(health.oversized_records, 0);
        assert_eq!(health.oversized_reports, 0);
        assert_eq!(health.folder.state, FolderState::Idle);
        assert!(harness.handle.take_folder_result().is_none());
    }
}

#[test]
fn off_startup_preserves_preexisting_segments_lock_and_metadata_without_repair() {
    fn metadata(path: &Path) -> (u64, u64, u32, i64, i64, i64, i64, i64, i64) {
        let value = fs::metadata(path).unwrap();
        (
            value.ino(),
            value.len(),
            value.mode(),
            value.atime(),
            value.atime_nsec(),
            value.mtime(),
            value.mtime_nsec(),
            value.ctime(),
            value.ctime_nsec(),
        )
    }
    let directory = Directory::new();
    let mut seeded = Harness::new(directory.config());
    publish(&seeded.handle, report(1, "Live"));
    seeded.event(1);
    seeded.drain();
    drop(seeded);
    let segment = directory.logs().join("furami.log");
    fs::OpenOptions::new()
        .append(true)
        .open(&segment)
        .unwrap()
        .write_all(b"{\"incomplete_tail\":")
        .unwrap();
    let lock_path = directory.logs().join(".furami-log.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    lock.lock().unwrap();
    let bytes = fs::read(&segment).unwrap();
    let before = (
        metadata(&directory.logs()),
        metadata(&segment),
        metadata(&lock_path),
    );
    let mut harness = Harness::new(DiagnosticsConfig {
        logging_enabled: false,
        ..directory.config()
    });
    publish(&harness.handle, report(1, "Paused"));
    harness.event(2);
    harness.handle.prepare_log_folder(FolderRequestId(1));
    harness.drain();
    assert_eq!(
        before,
        (
            metadata(&directory.logs()),
            metadata(&segment),
            metadata(&lock_path)
        )
    );
    assert_eq!(fs::read(&segment).unwrap(), bytes);
    assert!(harness.console.0.lock().is_empty());
    assert_eq!(harness.handle.health().last_error, None);
}

#[test]
fn rapid_off_on_discards_queued_epochs_and_emits_only_current_checkpoint() {
    let directory = Directory::new();
    let mut config = directory.config();
    config.limits.queue_messages = 4;
    let bounds = config.limits;
    let mut harness = Harness::new(config);
    publish(&harness.handle, report(1, "Opening"));
    harness.event(1);
    publish(&harness.handle, report(2, "Live"));
    harness.event(2);
    publish(&harness.handle, report(3, "Pausing"));
    harness.handle.prepare_log_folder(FolderRequestId(1));
    let before = harness.handle.health();
    harness.handle.set_logging_enabled(false).unwrap();
    publish(&harness.handle, report(4, "Paused"));
    harness.event(4);
    publish(&harness.handle, report(5, "Idle"));
    harness.handle.set_logging_enabled(true).unwrap();
    harness.drain();
    assert!(harness.handle.take_folder_result().is_none());
    assert!(!harness.handle.claim_log_folder_dispatch(FolderRequestId(1)));
    harness.event(5);
    harness.drain();
    let lines = records(&directory.logs().join("furami.log"));
    let checkpoints: Vec<_> = lines
        .iter()
        .filter(|line| line["record_type"] == "report")
        .collect();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0]["report_revision"], 5);
    assert_eq!(checkpoints[0]["report"]["session"]["phase"], "Idle");
    let events: Vec<_> = lines
        .iter()
        .filter(|line| line["record_type"] != "report" && line["record_type"] != "segment")
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["fields"]["apply_id"], 5);
    assert_eq!(events[0]["report_revision"], 5);
    let console = String::from_utf8(harness.console.0.lock().clone()).unwrap();
    let console: Vec<Value> = console
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(console.len(), 2);
    assert!(console.iter().all(|line| line["report_revision"] == 5));
    assert_eq!(
        harness.handle.health().dropped_events,
        before.dropped_events
    );
    assert_eq!(harness.handle.health().last_error, before.last_error);
    assert_bounds_and_context(&directory.logs(), bounds);
}

#[test]
fn disable_returns_while_claimed_storage_is_blocked_and_next_console_effect_is_cancelled() {
    for stage in [
        logging::WorkerStage::StorageClaimed,
        logging::WorkerStage::StorageFinished,
    ] {
        let directory = Directory::new();
        let mut harness = Harness::new(directory.config());
        publish(&harness.handle, report(1, "Live"));
        let (entered, resume) = harness.worker.pause_at(stage);
        let handle = harness.handle.clone();
        let console = harness.console.clone();
        let mut worker = harness.worker;
        let (worker, returned) = std::thread::scope(|scope| {
            let pending = scope.spawn(move || {
                worker.drain_available();
                worker
            });
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let control = handle.clone();
            scope.spawn(move || {
                done_tx.send(control.set_logging_enabled(false)).unwrap();
            });
            let returned = done_rx.recv_timeout(Duration::from_secs(5));
            resume.send(()).unwrap();
            (pending.join().unwrap(), returned)
        });
        returned
            .expect("disable does not wait for the claimed worker effect")
            .unwrap();
        assert!(
            console.0.lock().is_empty(),
            "unclaimed console effect was cancelled"
        );
        assert!(!handle.health().logging_enabled);
        assert!(!handle.health().has_problem());
        assert!(
            records(&directory.logs().join("furami.log"))
                .iter()
                .any(|line| line["report_revision"] == 1)
        );
        drop(worker);
    }
}

#[test]
fn late_claimed_storage_failure_is_retained_masked_and_not_retried_on_enable() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    publish(&harness.handle, report(1, "Live"));
    harness.worker.set_failpoint(Failpoint::DirectoryAccess);
    let (entered, resume) = harness
        .worker
        .pause_at(logging::WorkerStage::StorageClaimed);
    let handle = harness.handle.clone();
    let mut worker = harness.worker;
    worker = std::thread::scope(|scope| {
        let pending = scope.spawn(move || {
            worker.drain_available();
            worker
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.set_logging_enabled(false).unwrap();
        resume.send(()).unwrap();
        pending.join().unwrap()
    });
    let health = handle.health();
    assert_eq!(
        health.sink_error.unwrap().code,
        DiagnosticsCode::DirectoryAccess
    );
    assert_eq!(health.sink_status, SinkStatus::Unavailable);
    assert!(!health.has_problem());
    assert_eq!(health.message_fr(), "");
    handle.set_logging_enabled(true).unwrap();
    worker.drain_available();
    assert_eq!(handle.health().sink_error, health.sink_error);
    assert!(handle.health().has_problem());
    assert!(
        !directory.logs().exists(),
        "failed storage is not retried by the mode switch"
    );
}

#[test]
fn folder_disable_cancels_every_undispatched_stage_and_old_ack_without_reusing_ids() {
    for point in 0..4 {
        let directory = Directory::new();
        let mut harness = Harness::new(directory.config());
        let request = FolderRequestId(1);
        harness.handle.prepare_log_folder(request);
        if point > 0 {
            harness.drain();
        }
        if point > 1 {
            assert!(harness.handle.take_folder_result().unwrap().result.is_ok());
        }
        if point > 2 {
            assert!(harness.handle.claim_log_folder_dispatch(request));
        }
        harness.handle.set_logging_enabled(false).unwrap();
        harness.handle.set_logging_enabled(true).unwrap();
        harness.drain();
        assert!(harness.handle.take_folder_result().is_none());
        assert!(!harness.handle.claim_log_folder_dispatch(request));
        harness.handle.acknowledge_log_folder(request, false);
        assert_eq!(harness.handle.health().folder.error, None);
        assert_eq!(harness.handle.health().folder.state, FolderState::Idle);
        harness.handle.prepare_log_folder(request);
        assert_eq!(harness.handle.health().folder.state, FolderState::Idle);
        let next = FolderRequestId(2);
        harness.handle.prepare_log_folder(next);
        harness.drain();
        assert!(harness.handle.take_folder_result().unwrap().result.is_ok());
        assert!(!harness.handle.claim_log_folder_dispatch(request));
        assert!(harness.handle.claim_log_folder_dispatch(next));
        assert!(!harness.handle.claim_log_folder_dispatch(next));
        harness.handle.acknowledge_log_folder(request, false);
        assert_eq!(harness.handle.health().folder.state, FolderState::Preparing);
        harness.handle.acknowledge_log_folder(next, true);
        harness.handle.acknowledge_log_folder(next, false);
        assert_eq!(
            harness.handle.health().folder.state,
            FolderState::OpenRequested
        );
        assert_eq!(harness.handle.health().folder.error, None);
    }
}

#[test]
fn folder_result_from_claimed_preparation_is_cancelled_during_or_after_io() {
    for stage in [
        logging::WorkerStage::FolderClaimed,
        logging::WorkerStage::FolderFinished,
    ] {
        let directory = Directory::new();
        let mut harness = Harness::new(directory.config());
        harness.handle.prepare_log_folder(FolderRequestId(1));
        let (entered, resume) = harness.worker.pause_at(stage);
        let handle = harness.handle.clone();
        let mut worker = harness.worker;
        std::thread::scope(|scope| {
            let pending = scope.spawn(move || {
                worker.drain_available();
            });
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
            handle.set_logging_enabled(false).unwrap();
            handle.set_logging_enabled(true).unwrap();
            resume.send(()).unwrap();
            pending.join().unwrap();
        });
        assert!(handle.take_folder_result().is_none());
        assert!(!handle.claim_log_folder_dispatch(FolderRequestId(1)));
        handle.acknowledge_log_folder(FolderRequestId(1), false);
        assert_eq!(handle.health().folder.state, FolderState::Idle);
        assert_eq!(handle.health().folder.error, None);
    }
}

#[test]
fn claimed_console_failure_does_not_block_disable_and_stays_failed_after_enable() {
    struct BlockingFailure {
        entered: Option<std::sync::mpsc::Sender<()>>,
        resume: std::sync::mpsc::Receiver<()>,
        writes: Arc<AtomicU64>,
    }
    impl Write for BlockingFailure {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.resume.recv().unwrap();
            }
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let directory = Directory::new();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let writes = Arc::new(AtomicU64::new(0));
    let (handle, mut worker) = logging::test_pair(
        directory.config(),
        Box::new(BlockingFailure {
            entered: Some(entered_tx),
            resume: resume_rx,
            writes: Arc::clone(&writes),
        }),
    );
    publish(&handle, report(1, "Live"));
    handle.event(
        EventLevel::Info,
        "apply_runtime",
        &[SafeField::u64("apply_id", 1)],
    );
    let (mut worker, returned) = std::thread::scope(|scope| {
        let pending = scope.spawn(move || {
            worker.drain_available();
            worker
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let control = handle.clone();
        scope.spawn(move || {
            done_tx.send(control.set_logging_enabled(false)).unwrap();
        });
        let returned = done_rx.recv_timeout(Duration::from_secs(5));
        resume_tx.send(()).unwrap();
        (pending.join().unwrap(), returned)
    });
    returned
        .expect("disable returns while a claimed console write is held")
        .unwrap();
    let failed = handle.health();
    assert_eq!(
        failed.last_error.unwrap().code,
        DiagnosticsCode::ConsoleWriteFailed
    );
    assert_eq!(failed.sink_status, SinkStatus::Ready);
    assert!(!failed.has_problem());
    assert_eq!(failed.message_fr(), "");
    handle.set_logging_enabled(true).unwrap();
    handle.event(
        EventLevel::Info,
        "apply_runtime",
        &[SafeField::u64("apply_id", 2)],
    );
    worker.drain_available();
    assert_eq!(
        writes.load(Ordering::Relaxed),
        1,
        "failed console is not retried"
    );
    assert_eq!(handle.health().last_error, failed.last_error);
    assert!(handle.health().has_problem());
    let lines = records(&directory.logs().join("furami.log"));
    assert!(!lines.iter().any(|line| line["fields"]["apply_id"] == 1));
    assert!(lines.iter().any(|line| line["fields"]["apply_id"] == 2));
}

#[test]
fn disabling_preserves_completed_folder_failure_and_masked_diagnostic_warning() {
    let directory = Directory::new();
    let mut harness = Harness::new(directory.config());
    harness.handle.prepare_log_folder(FolderRequestId(1));
    harness.drain();
    harness.handle.take_folder_result().unwrap();
    assert!(harness.handle.claim_log_folder_dispatch(FolderRequestId(1)));
    harness
        .handle
        .acknowledge_log_folder(FolderRequestId(1), false);
    let failure = harness.handle.health().folder;
    assert_eq!(failure.error.unwrap().code, DiagnosticsCode::OpenRejected);
    harness.handle.set_logging_enabled(false).unwrap();
    assert_eq!(harness.handle.health().folder, failure);
    assert!(!harness.handle.health().has_problem());
    assert_eq!(harness.handle.health().message_fr(), "");
    harness.handle.set_logging_enabled(true).unwrap();
    assert_eq!(harness.handle.health().folder, failure);
    assert!(harness.handle.health().has_problem());
}
