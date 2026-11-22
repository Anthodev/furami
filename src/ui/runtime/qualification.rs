//! File-video application route. No capture catalog, persistence or audio services.
use super::*;
use crate::media::{
    gate::{QualificationGate, QualificationUpdate},
    qualification::QualificationVideo,
    render_catalog::CompiledRendererSettings,
};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QualificationCommand {
    Snapshot,
    Fullscreen,
    ApplicationClose,
    Pause(AttemptId),
}

impl QualificationCommand {
    pub(crate) fn parse(line: &str) -> Option<Self> {
        match line {
            "snapshot" => Some(Self::Snapshot),
            "fullscreen" => Some(Self::Fullscreen),
            "application-close" => Some(Self::ApplicationClose),
            _ => {
                let value = line.strip_prefix("pause ")?;
                if value.starts_with('0') || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                AttemptId::new(value.parse().ok()?).map(Self::Pause)
            }
        }
    }
}

pub(crate) fn log_qualification_rejection() {
    tracing::warn!(
        event_code = "qualification_command_rejected",
        kind = "CommandRejected",
        accepted = false
    );
}

pub(crate) struct QualificationRuntime {
    gate: QualificationGate,
    pending: Option<(String, QualificationVideo, CompiledRendererSettings)>,
    diagnostics: crate::diagnostics::DiagnosticsHandle,
    error: Option<projection::IntentError>,
    startup_failure: Option<crate::media::controller::MediaError>,
    fullscreen: bool,
    last_snapshot: Option<Rc<projection::Snapshot>>,
    last_backend: Option<crate::media::controller::RuntimeMediaObservation>,
    native_environment: crate::diagnostics::report::NativeEnvironmentReport,
}
impl QualificationRuntime {
    pub(super) fn new(
        prefix: String,
        video: QualificationVideo,
        renderer: CompiledRendererSettings,
        diagnostics: crate::diagnostics::DiagnosticsHandle,
    ) -> Self {
        Self {
            gate: QualificationGate::new(),
            pending: Some((prefix, video, renderer)),
            diagnostics,
            error: None,
            startup_failure: None,
            fullscreen: false,
            last_snapshot: None,
            last_backend: None,
            native_environment: Default::default(),
        }
    }
    pub(super) fn reject(&mut self, action: &'static str) -> UiUpdate {
        log_qualification_rejection();
        self.error = Some(projection::IntentError {
            code: "unavailable",
            action,
            field: "qualification",
            message: "Action unavailable during video qualification.".into(),
        });
        self.poll()
    }
    pub(super) fn ui_ready(&mut self) -> UiUpdate {
        if let Some((prefix, video, renderer)) = self.pending.take()
            && let Err(error) = self.gate.begin(prefix, video, renderer)
        {
            self.startup_failure = Some(error);
            self.gate.request_close();
        }
        self.poll()
    }
    pub(super) fn native_environment(
        &mut self,
        report: crate::diagnostics::report::NativeEnvironmentReport,
    ) -> UiUpdate {
        let changed = self.native_environment != report;
        if changed {
            self.native_environment = report;
        }
        self.poll_with_native_change(changed)
    }
    pub(super) fn request_application_close(&mut self) -> UiUpdate {
        self.pending = None;
        self.gate.request_close();
        self.poll()
    }
    pub(super) fn quit(&mut self) -> UiUpdate {
        self.request_application_close()
    }
    pub(super) fn close(&mut self, attempt: u64) -> UiUpdate {
        if attempt != 1 {
            return self.reject("close");
        }
        self.request_application_close()
    }
    pub(super) fn set_fullscreen(&mut self, fullscreen: bool) -> UiUpdate {
        self.fullscreen = fullscreen;
        self.poll()
    }
    pub(super) fn surface_ready(&mut self, token: SurfaceToken) -> UiUpdate {
        self.gate.surface_ready(token);
        self.poll()
    }
    pub(super) fn surface_lost(&mut self, attempt: AttemptId) -> UiUpdate {
        self.gate.surface_lost(attempt);
        self.poll()
    }
    pub(super) fn wait_for_owner_ack(&mut self, attempt: AttemptId) -> String {
        self.gate
            .wait_for_owner_ack(attempt)
            .err()
            .map(|error| error.code.to_owned())
            .unwrap_or_default()
    }
    #[cfg(target_os = "linux")]
    pub(super) fn owner_ack_failed(&self) -> bool {
        self.gate.owner_ack_failed()
    }
    pub(super) fn native_released(&mut self, attempt: AttemptId) -> UiUpdate {
        self.gate.native_released(attempt);
        self.poll()
    }
    pub(super) fn pause(&mut self, attempt: AttemptId) -> SubmitStatus {
        match self.gate.toggle_pause(attempt) {
            crate::media::controller::SubmitStatus::Accepted => SubmitStatus::Accepted,
            crate::media::controller::SubmitStatus::StaleGeneration => {
                SubmitStatus::StaleGeneration
            }
            crate::media::controller::SubmitStatus::NotReady => SubmitStatus::NotReady,
            crate::media::controller::SubmitStatus::Closing => SubmitStatus::Closing,
            crate::media::controller::SubmitStatus::CapacityExceeded => {
                SubmitStatus::CapacityExceeded
            }
        }
    }
    pub(super) fn qualification_command(&mut self, command: QualificationCommand) -> UiUpdate {
        let kind = match command {
            QualificationCommand::Snapshot => "Snapshot",
            QualificationCommand::ApplicationClose => "ApplicationClose",
            QualificationCommand::Pause(_) => "Pause",
            // Fullscreen requests must be handled by the native host. The
            // runtime only receives the observed state via set_fullscreen.
            QualificationCommand::Fullscreen => return self.reject("qualification_command"),
        };
        tracing::info!(event_code = "qualification_request_parsed", kind);
        match command {
            QualificationCommand::Snapshot => {
                self.last_snapshot = None;
                self.poll()
            }
            QualificationCommand::ApplicationClose => self.request_application_close(),
            QualificationCommand::Pause(attempt)
                if self.pause(attempt) == SubmitStatus::Accepted =>
            {
                self.poll()
            }
            _ => self.reject("qualification_command"),
        }
    }
    fn snapshot(&self, update: &QualificationUpdate) -> projection::Snapshot {
        let health = self.diagnostics.health();
        let diagnostics = projection::Diagnostics {
            status: match health.sink_status {
                crate::diagnostics::SinkStatus::Starting => "Starting",
                crate::diagnostics::SinkStatus::Ready if health.has_problem() => "Degraded",
                crate::diagnostics::SinkStatus::Ready => "Healthy",
                crate::diagnostics::SinkStatus::Unavailable => "Unavailable",
                crate::diagnostics::SinkStatus::Stopped => "Stopped",
            },
            path: health
                .location
                .as_ref()
                .map(|location| location.path.clone())
                .unwrap_or_default(),
            message: health.message(),
            has_problem: health.has_problem(),
            dropped_events: health.dropped_events.to_string(),
            oversized_records: health.oversized_records.to_string(),
            oversized_reports: health.oversized_reports.to_string(),
            folder_state: "Idle",
        };
        let closing = matches!(
            update.phase,
            GatePhase::Stopping | GatePhase::Releasing | GatePhase::QuitReady
        );
        projection::Snapshot {
            schema: 2,
            session_kind: projection::SessionKind::QualificationVideo,
            state_token: "state:1".into(),
            generation: "1".into(),
            product_phase: if closing { "Stopped" } else { "Active" }.into(),
            gate_phase: format!("{:?}", update.phase),
            paused: update.paused,
            opening: matches!(update.phase, GatePhase::WaitingSurface | GatePhase::Opening),
            closing,
            can_edit_selection: false,
            can_restart: false,
            can_reconnect: false,
            can_toggle_pause: update.can_toggle_pause,
            can_set_gain: false,
            selection_revision: "0".into(),
            selection: None,
            applied: None,
            last_valid: None,
            stream_stats: None,
            live: projection::Live {
                source_label: "Qualification video".into(),
                playback_status: if update.paused {
                    "Paused"
                } else if update.can_toggle_pause {
                    "Live"
                } else {
                    "Stopped"
                }
                .into(),
                audio_status: "Disabled".into(),
                ..Default::default()
            },
            catalog: projection::Catalog {
                revision: "0".into(),
                capability_status: "unknown".into(),
                ..Default::default()
            },
            renderer: projection::Renderer::default(),
            output: projection::Output {
                revision: "0".into(),
                ..Default::default()
            },
            recovery: projection::Recovery::default(),
            settings: projection::Settings {
                reset_token: "0".into(),
                ..Default::default()
            },
            diagnostics,
            intent_error: self.error.clone(),
        }
    }
    pub(super) fn poll(&mut self) -> UiUpdate {
        self.poll_with_native_change(false)
    }
    fn poll_with_native_change(&mut self, native_changed: bool) -> UiUpdate {
        let update = self.gate.poll();
        let failure = update
            .terminal_failure
            .as_ref()
            .or(self.startup_failure.as_ref());
        let snapshot = self.snapshot(&update);
        let changed = self.last_snapshot.as_deref() != Some(&snapshot);
        let phase = match update.phase {
            GatePhase::Idle => "Idle",
            GatePhase::WaitingSurface => "WaitingSurface",
            GatePhase::Opening => "Opening",
            GatePhase::Ready => "Ready",
            GatePhase::Stopping => "Stopping",
            GatePhase::Releasing => "Releasing",
            GatePhase::Failed => "Failed",
            GatePhase::QuitReady => "QuitReady",
        };
        let ui_snapshot = if changed {
            let snapshot = Rc::new(snapshot);
            self.last_snapshot = Some(Rc::clone(&snapshot));
            let size = update
                .observations
                .as_ref()
                .and_then(|facts| facts.decoded_size.as_ref());
            tracing::info!(
                event_code = "qualification_state",
                phase,
                generation = update.generation,
                can_toggle_pause = update.can_toggle_pause,
                paused = update.paused,
                configured = size.is_some(),
                width = size.map_or(0, |size| size.value.width()),
                height = size.map_or(0, |size| size.value.height()),
                failed = failure.is_some(),
                failure_code = failure.map(|error| error.code),
                quit = update.quit
            );
            Some(snapshot)
        } else {
            None
        };
        if (changed || native_changed)
            && let Some(revision) = self.diagnostics.reserve_report_revision()
        {
            let mut report = crate::diagnostics::report::ReportSnapshot::initial();
            report.revision = revision;
            report.environment.os = self.diagnostics.os_report();
            report.environment.log_location = self.diagnostics.health();
            report.environment.native = self.native_environment.clone();
            report.environment.runtime_media = update
                .diagnostics
                .as_ref()
                .map(crate::diagnostics::report::RuntimeMediaReport::from)
                .unwrap_or_default();
            report.session.phase = "QualificationVideo".into();
            report.session.gate_phase = phase;
            report.session.identity.generation = Some(1);
            self.diagnostics.publish_report(report);
        }
        if self.last_backend != update.diagnostics {
            self.last_backend.clone_from(&update.diagnostics);
            if let Some(observed) = &update.diagnostics {
                let (gpu_next, x11vk) = match &observed.backend {
                    crate::media::controller::BackendObservation::Observed {
                        video_output,
                        gpu_context,
                    } => (&**video_output == "gpu-next", &**gpu_context == "x11vk"),
                    _ => (false, false),
                };
                tracing::info!(
                    event_code = "qualification_backend",
                    generation = observed.generation,
                    gpu_next,
                    x11vk
                );
            }
        }
        UiUpdate {
            ui_snapshot,
            changed,
            log_folder_request: 0,
            log_folder_path: String::new(),
            phase: update.phase,
            generation: 1,
            restart_generation: 1,
            can_open: false,
            can_restart: false,
            product_phase: if update.quit { "Stopped" } else { "Active" }.into(),
            audio_status: "Disabled".into(),
            audio_diagnostic: String::new(),
            failed: failure.is_some(),
            diagnostic: failure
                .map(|error| error.code.to_owned())
                .unwrap_or_default(),
            context_diagnostic: failure
                .map(|error| error.code.to_owned())
                .unwrap_or_default(),
            paused: update.paused,
            prepared_paused: false,
            volume_percent: 0,
            muted: true,
            can_toggle_pause: update.can_toggle_pause,
            can_set_gain: false,
            playback_status: if update.paused {
                "Paused"
            } else if update.can_toggle_pause {
                "Live"
            } else {
                "Stopped"
            }
            .into(),
            create_native: update.create_native,
            release_native: update.release_native,
            quit: update.quit,
            terminal_failure: failure.is_some(),
            settings_status: String::new(),
            close_dialog: String::new(),
            fullscreen: self.fullscreen,
            stats_hidden: true,
            logging_enabled: true,
            closing: matches!(
                update.phase,
                GatePhase::Stopping | GatePhase::Releasing | GatePhase::QuitReady
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn runtime() -> QualificationRuntime {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "furami-ui-qualification-{}-{}.y4m",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::write(
            &path,
            b"YUV4MPEG2 W320 H180 F30:1 Ip A1:1 C420jpeg\nFRAME\n",
        )
        .unwrap();
        let video = QualificationVideo::open(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        let renderer = crate::media::render_catalog::compile_renderer(
            &RendererSettings::default(),
            &crate::media::render_catalog::fixture_capabilities(),
        )
        .unwrap();
        QualificationRuntime::new(
            String::new(),
            video,
            renderer,
            crate::diagnostics::DiagnosticsHandle::for_test(),
        )
    }
    #[test]
    fn qualification_projection_has_no_capture_identity_or_catalog_and_commands_cannot_create_one()
    {
        let mut runtime = runtime();
        let initial: serde_json::Value =
            serde_json::to_value(runtime.poll().ui_snapshot.as_deref().unwrap()).unwrap();
        assert_eq!(initial["session_kind"], "qualification-video");
        for field in ["selection", "applied", "lastValid", "streamStats"] {
            assert!(initial[field].is_null());
        }
        assert_eq!(initial["catalog"]["sourceRows"], serde_json::json!([]));
        assert_eq!(initial["output"]["rows"], serde_json::json!([]));
        assert_eq!(
            QualificationCommand::parse("fullscreen"),
            Some(QualificationCommand::Fullscreen)
        );
        for command in [
            "",
            "force-surface-loss",
            "reset-confirm",
            "open-close-during-opening",
            "open",
            "settings-reset",
            "preference-volume 40",
            "pause 01",
            "pause 1 trailing",
            "snapshot ",
            "fullscreen unexpected",
            "pause +1",
            "pause 0",
            "pause 18446744073709551616",
        ] {
            assert_eq!(QualificationCommand::parse(command), None, "{command}");
        }
        let pause = QualificationCommand::parse("pause 18446744073709551615").unwrap();
        assert_eq!(
            pause,
            QualificationCommand::Pause(AttemptId::new(u64::MAX).unwrap())
        );
        let update = runtime.qualification_command(pause);
        let snapshot = update.ui_snapshot.as_deref().unwrap();
        assert_eq!(snapshot.intent_error.as_ref().unwrap().code, "unavailable");
        assert_eq!(snapshot.gate_phase, "Idle");
        assert!(runtime.pending.is_some());
        assert!(!update.create_native && !update.release_native && !update.quit);
        let snapshot = runtime.qualification_command(QualificationCommand::Snapshot);
        assert!(snapshot.ui_snapshot.is_some());
        assert!(!snapshot.create_native);
        let closed = runtime.qualification_command(QualificationCommand::ApplicationClose);
        assert!(closed.quit && !closed.terminal_failure);
        assert!(runtime.pending.is_none());
    }

    #[test]
    fn qualification_native_environment_publishes_without_replaying_the_ui() {
        use crate::diagnostics::report::{NativeEnvironmentReport, Observation, UnavailableReason};

        let mut runtime = runtime();
        assert!(runtime.poll().ui_snapshot.is_some());
        assert!(runtime.poll().ui_snapshot.is_none());
        let before = runtime.diagnostics.reserve_report_revision().unwrap();
        let report = NativeEnvironmentReport {
            frontend: Observation::Observed {
                value: "gpui".into(),
            },
            window_backend: Observation::Observed {
                value: "x11".into(),
            },
            xwayland_version: Observation::Unavailable {
                reason: UnavailableReason::NotExposed,
            },
            ..Default::default()
        };
        let update = runtime.native_environment(report.clone());
        assert!(update.ui_snapshot.is_none());
        assert!(!update.create_native && !update.release_native && !update.quit);
        assert_eq!(runtime.native_environment, report);
        let published = before.next().unwrap();
        let after = published.next().unwrap();
        assert_eq!(runtime.diagnostics.reserve_report_revision(), Some(after));
        assert!(runtime.native_environment(report).ui_snapshot.is_none());
        assert_eq!(runtime.diagnostics.reserve_report_revision(), after.next());
    }

    #[test]
    fn gpui_host_qualification_republishes_only_on_change_and_snapshot_forces_one_replay() {
        let mut runtime = runtime();
        let first = runtime.poll();
        assert!(first.ui_snapshot.is_some());
        let retained = runtime.last_snapshot.clone().expect("retained snapshot");

        let unchanged = runtime.poll();
        assert!(
            unchanged.ui_snapshot.is_none(),
            "an identical projection is not republished"
        );
        assert!(Rc::ptr_eq(
            &retained,
            runtime.last_snapshot.as_ref().expect("retained snapshot")
        ));

        let forced = runtime.qualification_command(QualificationCommand::Snapshot);
        assert!(
            forced.ui_snapshot.is_some(),
            "explicit snapshot republishes"
        );
        assert!(runtime.poll().ui_snapshot.is_none());
    }
}
