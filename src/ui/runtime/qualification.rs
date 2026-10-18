//! File-video application route. No capture catalog, persistence or audio services.
use super::*;
use crate::media::{
    gate::{QualificationGate, QualificationUpdate},
    qualification::QualificationVideo,
    render_catalog::CompiledRendererSettings,
};

pub(crate) struct QualificationRuntime {
    gate: QualificationGate,
    pending: Option<(String, QualificationVideo, CompiledRendererSettings)>,
    diagnostics: crate::diagnostics::DiagnosticsHandle,
    error: Option<projection::IntentError>,
    startup_failure: Option<crate::media::controller::MediaError>,
    fullscreen: bool,
    last_snapshot: String,
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
            last_snapshot: String::new(),
            last_backend: None,
            native_environment: Default::default(),
        }
    }
    pub(super) fn reject(&mut self, action: &'static str) -> UiUpdate {
        tracing::warn!(
            event_code = "qualification_command_rejected",
            kind = "CommandRejected",
            accepted = false
        );
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
    #[allow(
        clippy::too_many_arguments,
        reason = "Mirrors the CXX native environment boundary."
    )]
    pub(super) fn native_environment(
        &mut self,
        qt: &str,
        platform: &str,
        vendor: &str,
        release: u32,
        protocol_major: u32,
        protocol_minor: u32,
        shape_major: u32,
        shape_minor: u32,
    ) -> UiUpdate {
        use crate::diagnostics::report::{NativeEnvironmentReport, Observation, XServerReport};
        self.native_environment = NativeEnvironmentReport {
            qt_runtime_version: Observation::Observed {
                value: crate::diagnostics::report::bounded_text(qt, 128),
            },
            qt_platform: Observation::Observed {
                value: crate::diagnostics::report::bounded_text(platform, 128),
            },
            x_server: Observation::Observed {
                value: XServerReport {
                    vendor: crate::diagnostics::report::bounded_text(vendor, 256),
                    release,
                    protocol_major,
                    protocol_minor,
                    shape_major,
                    shape_minor,
                },
            },
            ..Default::default()
        };
        self.poll()
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
    pub(super) fn qualification_command(&mut self, line: &str) -> UiUpdate {
        // No trim: the finite command grammar does not admit hidden trailing input.
        tracing::info!(
            event_code = "qualification_request_parsed",
            kind = qualification_intent(line)
        );
        match line {
            "snapshot" => {
                self.last_snapshot.clear();
                self.poll()
            }
            "fullscreen" => self.set_fullscreen(!self.fullscreen),
            "application-close" => self.request_application_close(),
            _ => {
                if let Some(value) = line.strip_prefix("pause ")
                    && let Ok(generation) = value.parse::<u64>()
                    && generation.to_string() == value
                    && let Some(attempt) = AttemptId::new(generation)
                    && self.pause(attempt) == SubmitStatus::Accepted
                {
                    return self.poll();
                }
                self.reject("qualification_command")
            }
        }
    }
    pub(super) fn unchanged(&mut self) -> UiUpdate {
        self.poll()
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
        let update = self.gate.poll();
        let failure = update
            .terminal_failure
            .as_ref()
            .or(self.startup_failure.as_ref());
        let snapshot = serde_json::to_string(&self.snapshot(&update))
            .expect("finite qualification projection");
        let changed = snapshot != self.last_snapshot;
        if changed {
            self.last_snapshot.clone_from(&snapshot);
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
                quit = update.quit
            );
            if let Some(revision) = self.diagnostics.reserve_report_revision() {
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
            ui_snapshot: if changed { snapshot } else { String::new() },
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
            audio_source: String::new(),
            audio_desired: String::new(),
            audio_diagnostic: String::new(),
            failed: failure.is_some(),
            diagnostic: failure
                .map(|error| error.code.to_owned())
                .unwrap_or_default(),
            context_diagnostic: failure
                .map(|error| error.code.to_owned())
                .unwrap_or_default(),
            recovery_evidence: String::new(),
            recovery_stage: String::new(),
            candidates: Vec::new(),
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
            settings_path: String::new(),
            settings_refused: false,
            saved_selection: String::new(),
            startup_reason: String::new(),
            close_dialog: String::new(),
            reset_token: 0,
            fullscreen: self.fullscreen,
            stats_hidden: true,
            logging_enabled: true,
            closing: matches!(
                update.phase,
                GatePhase::Stopping | GatePhase::Releasing | GatePhase::QuitReady
            ),
            output_rows: "[]".into(),
            output_catalog_revision: 0,
            output_selected: String::new(),
            output_selected_key: String::new(),
            output_effective: String::new(),
            output_status: "Disabled".into(),
            output_needs_action: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn runtime() -> QualificationRuntime {
        let path = std::env::temp_dir().join(format!(
            "furami-ui-qualification-{}.y4m",
            std::process::id()
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
        let initial: serde_json::Value = serde_json::from_str(&runtime.poll().ui_snapshot).unwrap();
        assert_eq!(initial["session_kind"], "qualification-video");
        for field in ["selection", "applied", "lastValid", "streamStats"] {
            assert!(initial[field].is_null());
        }
        assert_eq!(initial["catalog"]["sourceRows"], serde_json::json!([]));
        assert_eq!(initial["output"]["rows"], serde_json::json!([]));
        for command in [
            "force-surface-loss",
            "reset-confirm",
            "open-close-during-opening",
            "open",
            "settings-reset",
            "preference-volume 40",
            "pause 01",
            "pause 1 trailing",
            "snapshot ",
        ] {
            let update = runtime.qualification_command(command);
            let snapshot: serde_json::Value =
                serde_json::from_str(if update.ui_snapshot.is_empty() {
                    &runtime.last_snapshot
                } else {
                    &update.ui_snapshot
                })
                .unwrap();
            assert_eq!(snapshot["intentError"]["code"], "unavailable", "{command}");
            assert_eq!(snapshot["gatePhase"], "Idle", "{command}");
            assert!(runtime.pending.is_some());
            assert!(!update.create_native && !update.release_native && !update.quit);
        }
        let snapshot = runtime.qualification_command("snapshot");
        assert!(!snapshot.ui_snapshot.is_empty());
        assert!(!snapshot.create_native);
        let closed = runtime.request_application_close();
        assert!(closed.quit && !closed.terminal_failure);
        assert!(runtime.pending.is_none());
    }
}
