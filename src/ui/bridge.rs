//! Value-only CXX boundary. C++ retains every Qt object and notification target.

use super::runtime::{self, RuntimeCoordinator};
use crate::diagnostics::{diagnostics_native_event, diagnostics_qt_message};
use crate::{
    app::{gate::GatePhase, ports::SubmitStatus},
    domain::state::AttemptId,
    media::controller::{Generation, SurfaceToken, X11WindowId},
};

#[cxx_qt::bridge(namespace = "furami::bridge")]
pub(crate) mod ffi {
    #[derive(Debug)]
    struct LaunchResult {
        exit_code: i32,
        diagnostic: String,
    }
    #[derive(Debug)]
    enum GatePhase {
        Idle,
        WaitingSurface,
        Opening,
        Ready,
        Stopping,
        Releasing,
        Failed,
        QuitReady,
    }
    #[derive(Debug)]
    enum SubmitStatus {
        Accepted,
        StaleGeneration,
        NotReady,
        Closing,
        CapacityExceeded,
    }
    #[derive(Debug)]
    struct UiUpdate {
        ui_snapshot: String,
        log_folder_request: u64,
        log_folder_path: String,
        changed: bool,
        phase: GatePhase,
        generation: u64,
        restart_generation: u64,
        can_open: bool,
        can_restart: bool,
        product_phase: String,
        audio_status: String,
        audio_diagnostic: String,
        audio_source: String,
        audio_desired: String,
        failed: bool,
        diagnostic: String,
        context_diagnostic: String,
        recovery_evidence: String,
        recovery_stage: String,
        candidates: String,
        paused: bool,
        prepared_paused: bool,
        volume_percent: i32,
        muted: bool,
        can_toggle_pause: bool,
        can_set_gain: bool,
        playback_status: String,
        create_native: bool,
        release_native: bool,
        quit: bool,
        settings_status: String,
        settings_path: String,
        settings_refused: bool,
        saved_selection: String,
        startup_reason: String,
        close_dialog: String,
        reset_token: u64,
        fullscreen: bool,
        stats_hidden: bool,
        logging_enabled: bool,
        closing: bool,
        output_rows: String,
        output_catalog_revision: u64,
        output_selected: String,
        output_selected_key: String,
        output_effective: String,
        output_status: String,
        output_needs_action: bool,
    }
    // SAFETY: host.h declares this exact ABI. Rust owns only values and adapters;
    // C++ owns Qt lifetime through authorized event-loop exit.
    unsafe extern "C++" {
        include!("host.h");
        fn run_qt_application(
            gate: Box<RuntimeCoordinator>,
            display: &str,
            qualification_stdin: bool,
        ) -> LaunchResult;
    }
    extern "Rust" {
        type RuntimeCoordinator;
        fn diagnostics_qt_message(severity: i32);
        fn diagnostics_native_event(code: &str, severity: i32, fields_json: &str);
        // Local exception: this declaration mirrors `host.h`, where the nine
        // environment-bootstrap values cross the CXX boundary positionally in a
        // single auditable call; bundling them would change the frozen native
        // ABI and the existing ownership-bootstrap handoff.
        #[allow(clippy::too_many_arguments, reason = "CXX positional boundary.")]
        fn gate_native_environment(
            gate: &mut RuntimeCoordinator,
            qt_version: &str,
            platform: &str,
            x_vendor: &str,
            x_release: u32,
            x_protocol_major: u32,
            x_protocol_minor: u32,
            shape_major: u32,
            shape_minor: u32,
        ) -> UiUpdate;
        fn gate_refresh_catalogs(gate: &mut RuntimeCoordinator) -> UiUpdate;
        fn gate_open_log_folder(gate: &mut RuntimeCoordinator) -> UiUpdate;
        fn gate_claim_log_folder_dispatch(gate: &mut RuntimeCoordinator, request_id: u64) -> bool;
        fn gate_log_folder_result(
            gate: &mut RuntimeCoordinator,
            request_id: u64,
            accepted: bool,
        ) -> UiUpdate;
        fn gate_log_folder_finished(
            gate: &mut RuntimeCoordinator,
            request_id: u64,
            failed: bool,
        ) -> UiUpdate;
        fn gate_select_capture(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            catalog_revision: &str,
            row_key: &str,
        ) -> UiUpdate;
        fn gate_select_size(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            catalog_revision: &str,
            row_key: &str,
        ) -> UiUpdate;
        fn gate_set_size(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            catalog_revision: &str,
            width: &str,
            height: &str,
        ) -> UiUpdate;
        fn gate_select_rate(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            catalog_revision: &str,
            row_key: &str,
        ) -> UiUpdate;
        fn gate_set_rate(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            catalog_revision: &str,
            exact_rate: &str,
        ) -> UiUpdate;
        fn gate_select_format(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            catalog_revision: &str,
            row_key: &str,
        ) -> UiUpdate;
        fn gate_set_audio_enabled(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            enabled: bool,
        ) -> UiUpdate;
        fn gate_select_audio_input(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            catalog_revision: &str,
            row_key: &str,
        ) -> UiUpdate;
        fn gate_set_scale(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            kernel: &str,
        ) -> UiUpdate;
        fn gate_set_cscale(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            kernel: &str,
        ) -> UiUpdate;
        fn gate_set_dscale(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            kernel: &str,
        ) -> UiUpdate;
        fn gate_set_scale_antiring(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            value: f64,
        ) -> UiUpdate;
        fn gate_set_dither(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            mode: &str,
        ) -> UiUpdate;
        fn gate_set_deband_enabled(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            enabled: bool,
        ) -> UiUpdate;
        fn gate_set_deband_iterations(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            value: i32,
        ) -> UiUpdate;
        fn gate_set_deband_grain(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            value: f64,
        ) -> UiUpdate;
        fn gate_set_interpolation(
            gate: &mut RuntimeCoordinator,
            selection_revision: &str,
            enabled: bool,
        ) -> UiUpdate;
        fn gate_ui_ready(gate: &mut RuntimeCoordinator) -> UiUpdate;
        fn gate_request_application_close(gate: &mut RuntimeCoordinator) -> UiUpdate;
        fn gate_retry_save(gate: &mut RuntimeCoordinator) -> UiUpdate;
        fn gate_close_without_save(gate: &mut RuntimeCoordinator) -> UiUpdate;
        fn gate_request_reset(gate: &mut RuntimeCoordinator) -> UiUpdate;
        fn gate_decide_reset(
            gate: &mut RuntimeCoordinator,
            token: u64,
            confirmed: bool,
        ) -> UiUpdate;
        fn gate_set_fullscreen(gate: &mut RuntimeCoordinator, fullscreen: bool) -> UiUpdate;
        fn gate_set_stats_hidden(gate: &mut RuntimeCoordinator, hidden: bool) -> UiUpdate;
        fn gate_set_logging_enabled(gate: &mut RuntimeCoordinator, enabled: bool) -> UiUpdate;
        fn gate_open(gate: &mut RuntimeCoordinator) -> UiUpdate;
        fn gate_restart(gate: &mut RuntimeCoordinator, generation: u64) -> UiUpdate;
        fn gate_reconnect(gate: &mut RuntimeCoordinator, generation: u64) -> UiUpdate;
        fn gate_choose_recovery(
            gate: &mut RuntimeCoordinator,
            generation: u64,
            watch: u64,
            epoch: u64,
            candidate: u64,
        ) -> UiUpdate;
        fn gate_qualification_command(gate: &mut RuntimeCoordinator, line: &str) -> UiUpdate;
        fn gate_surface_ready(gate: &mut RuntimeCoordinator, generation: u64, xid: u64)
        -> UiUpdate;
        fn gate_surface_lost(gate: &mut RuntimeCoordinator, generation: u64) -> UiUpdate;
        fn gate_wait_for_owner_ack(gate: &mut RuntimeCoordinator, generation: u64) -> String;
        fn gate_pause(gate: &mut RuntimeCoordinator, generation: u64) -> SubmitStatus;
        fn gate_set_volume(gate: &mut RuntimeCoordinator, percent: i32) -> SubmitStatus;
        fn gate_set_muted(gate: &mut RuntimeCoordinator, muted: bool) -> SubmitStatus;
        fn gate_select_output(
            gate: &mut RuntimeCoordinator,
            row_key: &str,
            catalog_revision: u64,
        ) -> SubmitStatus;
        fn gate_close(
            gate: &mut RuntimeCoordinator,
            generation: u64,
            application: bool,
        ) -> UiUpdate;
        fn gate_quit(gate: &mut RuntimeCoordinator) -> UiUpdate;
        fn gate_native_released(gate: &mut RuntimeCoordinator, generation: u64) -> UiUpdate;
        fn gate_poll(gate: &mut RuntimeCoordinator) -> UiUpdate;
        fn display_check(platform: &str, captured: &str, qt_display: &str, current: &str)
        -> String;
    }
}
impl From<runtime::UiUpdate> for ffi::UiUpdate {
    fn from(update: runtime::UiUpdate) -> Self {
        Self {
            ui_snapshot: update.ui_snapshot,
            log_folder_request: update.log_folder_request,
            log_folder_path: update.log_folder_path,
            changed: update.changed,
            phase: match update.phase {
                GatePhase::Idle => ffi::GatePhase::Idle,
                GatePhase::WaitingSurface => ffi::GatePhase::WaitingSurface,
                GatePhase::Opening => ffi::GatePhase::Opening,
                GatePhase::Ready => ffi::GatePhase::Ready,
                GatePhase::Stopping => ffi::GatePhase::Stopping,
                GatePhase::Releasing => ffi::GatePhase::Releasing,
                GatePhase::Failed => ffi::GatePhase::Failed,
                GatePhase::QuitReady => ffi::GatePhase::QuitReady,
            },
            generation: update.generation,
            restart_generation: update.restart_generation,
            can_open: update.can_open,
            can_restart: update.can_restart,
            product_phase: update.product_phase,
            audio_status: update.audio_status,
            audio_diagnostic: update.audio_diagnostic,
            audio_source: update.audio_source,
            audio_desired: update.audio_desired,
            failed: update.failed,
            diagnostic: update.diagnostic,
            context_diagnostic: update.context_diagnostic,
            recovery_evidence: update.recovery_evidence,
            recovery_stage: update.recovery_stage,
            // Candidate entries are newline-joined; C++ splits for the
            // choice surface. Entry layout is pipe-separated technical
            // identity plus description.
            candidates: update.candidates.join("\n"),
            paused: update.paused,
            prepared_paused: update.prepared_paused,
            volume_percent: update.volume_percent,
            muted: update.muted,
            can_toggle_pause: update.can_toggle_pause,
            can_set_gain: update.can_set_gain,
            playback_status: update.playback_status,
            create_native: update.create_native,
            release_native: update.release_native,
            quit: update.quit,
            settings_status: update.settings_status,
            settings_path: update.settings_path,
            settings_refused: update.settings_refused,
            saved_selection: update.saved_selection,
            startup_reason: update.startup_reason,
            close_dialog: update.close_dialog,
            reset_token: update.reset_token,
            fullscreen: update.fullscreen,
            stats_hidden: update.stats_hidden,
            logging_enabled: update.logging_enabled,
            closing: update.closing,
            output_rows: update.output_rows,
            output_catalog_revision: update.output_catalog_revision,
            output_selected: update.output_selected,
            output_selected_key: update.output_selected_key,
            output_effective: update.output_effective,
            output_status: update.output_status,
            output_needs_action: update.output_needs_action,
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn gate_native_environment(
    gate: &mut RuntimeCoordinator,
    qt_version: &str,
    platform: &str,
    x_vendor: &str,
    x_release: u32,
    x_protocol_major: u32,
    x_protocol_minor: u32,
    shape_major: u32,
    shape_minor: u32,
) -> ffi::UiUpdate {
    gate.native_environment(
        qt_version,
        platform,
        x_vendor,
        x_release,
        x_protocol_major,
        x_protocol_minor,
        shape_major,
        shape_minor,
    )
    .into()
}
fn gate_refresh_catalogs(gate: &mut RuntimeCoordinator) -> ffi::UiUpdate {
    gate.refresh_catalogs().into()
}
fn gate_select_capture(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    catalog_revision: &str,
    row_key: &str,
) -> ffi::UiUpdate {
    gate.select_capture(selection_revision, catalog_revision, row_key)
        .into()
}
fn gate_select_size(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    catalog_revision: &str,
    row_key: &str,
) -> ffi::UiUpdate {
    gate.select_size(selection_revision, catalog_revision, row_key)
        .into()
}
fn gate_set_size(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    catalog_revision: &str,
    width: &str,
    height: &str,
) -> ffi::UiUpdate {
    gate.set_size(selection_revision, catalog_revision, width, height)
        .into()
}
fn gate_select_rate(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    catalog_revision: &str,
    row_key: &str,
) -> ffi::UiUpdate {
    gate.select_rate(selection_revision, catalog_revision, row_key)
        .into()
}
fn gate_set_rate(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    catalog_revision: &str,
    exact_rate: &str,
) -> ffi::UiUpdate {
    gate.set_rate(selection_revision, catalog_revision, exact_rate)
        .into()
}
fn gate_select_format(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    catalog_revision: &str,
    row_key: &str,
) -> ffi::UiUpdate {
    gate.select_format(selection_revision, catalog_revision, row_key)
        .into()
}
fn gate_set_audio_enabled(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    enabled: bool,
) -> ffi::UiUpdate {
    gate.set_audio_enabled(selection_revision, enabled).into()
}
fn gate_select_audio_input(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    catalog_revision: &str,
    row_key: &str,
) -> ffi::UiUpdate {
    gate.select_audio_input(selection_revision, catalog_revision, row_key)
        .into()
}
fn gate_set_scale(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    kernel: &str,
) -> ffi::UiUpdate {
    gate.set_scale(selection_revision, kernel).into()
}
fn gate_set_cscale(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    kernel: &str,
) -> ffi::UiUpdate {
    gate.set_cscale(selection_revision, kernel).into()
}
fn gate_set_dscale(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    kernel: &str,
) -> ffi::UiUpdate {
    gate.set_dscale(selection_revision, kernel).into()
}
fn gate_set_scale_antiring(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    value: f64,
) -> ffi::UiUpdate {
    gate.set_scale_antiring(selection_revision, value).into()
}
fn gate_set_dither(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    mode: &str,
) -> ffi::UiUpdate {
    gate.set_dither(selection_revision, mode).into()
}
fn gate_set_deband_enabled(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    enabled: bool,
) -> ffi::UiUpdate {
    gate.set_deband_enabled(selection_revision, enabled).into()
}
fn gate_set_deband_iterations(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    value: i32,
) -> ffi::UiUpdate {
    gate.set_deband_iterations(selection_revision, value).into()
}
fn gate_set_deband_grain(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    value: f64,
) -> ffi::UiUpdate {
    gate.set_deband_grain(selection_revision, value).into()
}
fn gate_set_interpolation(
    gate: &mut RuntimeCoordinator,
    selection_revision: &str,
    enabled: bool,
) -> ffi::UiUpdate {
    gate.set_interpolation(selection_revision, enabled).into()
}
fn gate_ui_ready(gate: &mut RuntimeCoordinator) -> ffi::UiUpdate {
    gate.ui_ready().into()
}
fn gate_request_application_close(gate: &mut RuntimeCoordinator) -> ffi::UiUpdate {
    gate.request_application_close().into()
}
fn gate_retry_save(gate: &mut RuntimeCoordinator) -> ffi::UiUpdate {
    gate.retry_save().into()
}
fn gate_close_without_save(gate: &mut RuntimeCoordinator) -> ffi::UiUpdate {
    gate.close_without_save().into()
}
fn gate_request_reset(gate: &mut RuntimeCoordinator) -> ffi::UiUpdate {
    gate.request_reset().into()
}
fn gate_decide_reset(gate: &mut RuntimeCoordinator, token: u64, confirmed: bool) -> ffi::UiUpdate {
    gate.decide_reset(token, confirmed).into()
}
fn gate_set_fullscreen(gate: &mut RuntimeCoordinator, fullscreen: bool) -> ffi::UiUpdate {
    gate.set_fullscreen(fullscreen).into()
}
fn gate_set_stats_hidden(gate: &mut RuntimeCoordinator, hidden: bool) -> ffi::UiUpdate {
    gate.set_stats_hidden(hidden).into()
}
fn gate_set_logging_enabled(gate: &mut RuntimeCoordinator, enabled: bool) -> ffi::UiUpdate {
    gate.set_logging_enabled(enabled).into()
}
fn gate_open_log_folder(gate: &mut RuntimeCoordinator) -> ffi::UiUpdate {
    gate.open_log_folder().into()
}
fn gate_claim_log_folder_dispatch(gate: &mut RuntimeCoordinator, request_id: u64) -> bool {
    gate.claim_log_folder_dispatch(request_id)
}
fn gate_log_folder_result(
    gate: &mut RuntimeCoordinator,
    request_id: u64,
    accepted: bool,
) -> ffi::UiUpdate {
    gate.log_folder_result(request_id, accepted).into()
}
fn gate_log_folder_finished(
    gate: &mut RuntimeCoordinator,
    request_id: u64,
    failed: bool,
) -> ffi::UiUpdate {
    gate.log_folder_finished(request_id, failed).into()
}
fn gate_open(gate: &mut RuntimeCoordinator) -> ffi::UiUpdate {
    gate.open().into()
}
fn gate_restart(gate: &mut RuntimeCoordinator, generation: u64) -> ffi::UiUpdate {
    gate.restart(generation).into()
}
fn gate_reconnect(gate: &mut RuntimeCoordinator, generation: u64) -> ffi::UiUpdate {
    gate.reconnect(generation).into()
}
#[allow(clippy::too_many_arguments)]
fn gate_choose_recovery(
    gate: &mut RuntimeCoordinator,
    generation: u64,
    watch: u64,
    epoch: u64,
    candidate: u64,
) -> ffi::UiUpdate {
    gate.choose_recovery(generation, watch, epoch, candidate)
        .into()
}
fn gate_qualification_command(gate: &mut RuntimeCoordinator, line: &str) -> ffi::UiUpdate {
    gate.qualification_command(line).into()
}
fn gate_surface_ready(gate: &mut RuntimeCoordinator, generation: u64, xid: u64) -> ffi::UiUpdate {
    match (Generation::new(generation), X11WindowId::new(xid)) {
        (Some(generation), Some(xid)) => {
            gate.surface_ready(SurfaceToken { generation, xid }).into()
        }
        _ => gate.unchanged().into(),
    }
}
fn gate_surface_lost(gate: &mut RuntimeCoordinator, generation: u64) -> ffi::UiUpdate {
    match AttemptId::new(generation) {
        Some(attempt) => gate.surface_lost(attempt).into(),
        None => gate.unchanged().into(),
    }
}
fn gate_wait_for_owner_ack(gate: &mut RuntimeCoordinator, generation: u64) -> String {
    match AttemptId::new(generation) {
        Some(attempt) => gate.wait_for_owner_ack(attempt),
        None => "surface_loss_barrier: invalid generation".into(),
    }
}
fn submit_status(status: SubmitStatus) -> ffi::SubmitStatus {
    match status {
        SubmitStatus::Accepted => ffi::SubmitStatus::Accepted,
        SubmitStatus::StaleGeneration => ffi::SubmitStatus::StaleGeneration,
        SubmitStatus::NotReady => ffi::SubmitStatus::NotReady,
        SubmitStatus::Closing => ffi::SubmitStatus::Closing,
        SubmitStatus::CapacityExceeded => ffi::SubmitStatus::CapacityExceeded,
    }
}
fn gate_pause(gate: &mut RuntimeCoordinator, generation: u64) -> ffi::SubmitStatus {
    let status = AttemptId::new(generation)
        .map(|attempt| gate.pause(attempt))
        .unwrap_or(SubmitStatus::StaleGeneration);
    submit_status(status)
}
fn gate_set_volume(gate: &mut RuntimeCoordinator, percent: i32) -> ffi::SubmitStatus {
    submit_status(gate.set_volume(percent))
}
fn gate_set_muted(gate: &mut RuntimeCoordinator, muted: bool) -> ffi::SubmitStatus {
    submit_status(gate.set_muted(muted))
}
fn gate_select_output(
    gate: &mut RuntimeCoordinator,
    row_key: &str,
    catalog_revision: u64,
) -> ffi::SubmitStatus {
    submit_status(gate.select_output(row_key, catalog_revision))
}
fn gate_close(gate: &mut RuntimeCoordinator, generation: u64, application: bool) -> ffi::UiUpdate {
    if application {
        gate.request_application_close()
    } else {
        gate.close(generation)
    }
    .into()
}
fn gate_quit(gate: &mut RuntimeCoordinator) -> ffi::UiUpdate {
    gate.quit().into()
}
fn gate_native_released(gate: &mut RuntimeCoordinator, generation: u64) -> ffi::UiUpdate {
    match AttemptId::new(generation) {
        Some(attempt) => gate.native_released(attempt).into(),
        None => gate.unchanged().into(),
    }
}
fn gate_poll(gate: &mut RuntimeCoordinator) -> ffi::UiUpdate {
    gate.poll().into()
}
fn display_check(platform: &str, captured: &str, qt_display: &str, current: &str) -> String {
    crate::native_host::display_check(platform, captured, qt_display, current)
}
