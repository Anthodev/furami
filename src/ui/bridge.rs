//! Scalar-only CXX boundary. C++ retains every Qt object and notification target.

use crate::app::gate::{self, GateCoordinator, Generation, SurfaceToken, X11WindowId};

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
        changed: bool,
        phase: GatePhase,
        generation: u64,
        failed: bool,
        diagnostic: String,
        paused: bool,
        ended: bool,
        create_native: bool,
        release_native: bool,
        quit: bool,
    }

    // SAFETY: host.h declares this exact scalar-only C++ ABI. The implementation
    // owns Qt lifetime through event-loop exit and returns only copied values.
    unsafe extern "C++" {
        include!("host.h");
        fn run_qt_application(media_prefix: &str, display: &str) -> LaunchResult;
    }

    extern "Rust" {
        type GateCoordinator;
        fn new_gate(media_prefix: &str) -> Box<GateCoordinator>;
        fn gate_open(gate: &mut GateCoordinator) -> UiUpdate;
        fn gate_surface_ready(gate: &mut GateCoordinator, generation: u64, xid: u64) -> UiUpdate;
        fn gate_surface_lost(gate: &mut GateCoordinator, generation: u64) -> UiUpdate;
        fn gate_wait_for_owner_ack(gate: &mut GateCoordinator, generation: u64) -> String;
        fn gate_pause(gate: &mut GateCoordinator, generation: u64) -> SubmitStatus;
        fn gate_close(gate: &mut GateCoordinator, generation: u64, application: bool) -> UiUpdate;
        fn gate_quit(gate: &mut GateCoordinator) -> UiUpdate;
        fn gate_native_released(gate: &mut GateCoordinator, generation: u64) -> UiUpdate;
        fn gate_poll(gate: &mut GateCoordinator) -> UiUpdate;
        fn display_check(platform: &str, captured: &str, qt_display: &str, current: &str)
        -> String;
    }
}

impl From<gate::UiUpdate> for ffi::UiUpdate {
    fn from(update: gate::UiUpdate) -> Self {
        let phase = match update.phase {
            gate::GatePhase::Idle => ffi::GatePhase::Idle,
            gate::GatePhase::WaitingSurface => ffi::GatePhase::WaitingSurface,
            gate::GatePhase::Opening => ffi::GatePhase::Opening,
            gate::GatePhase::Ready => ffi::GatePhase::Ready,
            gate::GatePhase::Stopping => ffi::GatePhase::Stopping,
            gate::GatePhase::Releasing => ffi::GatePhase::Releasing,
            gate::GatePhase::Failed => ffi::GatePhase::Failed,
            gate::GatePhase::QuitReady => ffi::GatePhase::QuitReady,
        };
        Self {
            changed: update.changed,
            phase,
            generation: update.generation.map(Generation::get).unwrap_or(0),
            failed: update.failed,
            diagnostic: update.diagnostic,
            paused: update.paused,
            ended: update.ended,
            create_native: update.create_native,
            release_native: update.release_native,
            quit: update.quit,
        }
    }
}

fn new_gate(media_prefix: &str) -> Box<GateCoordinator> {
    Box::new(GateCoordinator::new(media_prefix.to_owned()))
}
fn gate_open(gate: &mut GateCoordinator) -> ffi::UiUpdate {
    gate.open().into()
}
fn gate_surface_ready(gate: &mut GateCoordinator, generation: u64, xid: u64) -> ffi::UiUpdate {
    match (Generation::new(generation), X11WindowId::new(xid)) {
        (Some(generation), Some(xid)) => {
            gate.surface_ready(SurfaceToken { generation, xid }).into()
        }
        _ => gate.unchanged().into(),
    }
}
fn gate_surface_lost(gate: &mut GateCoordinator, generation: u64) -> ffi::UiUpdate {
    Generation::new(generation)
        .map(|g| gate.surface_lost(g))
        .unwrap_or_else(|| gate.unchanged())
        .into()
}
fn gate_wait_for_owner_ack(gate: &mut GateCoordinator, generation: u64) -> String {
    let Some(generation) = Generation::new(generation) else {
        return "surface_loss_barrier: invalid generation".to_owned();
    };
    match gate.wait_for_owner_ack(generation) {
        Ok(()) => String::new(),
        Err(error) => error.to_string(),
    }
}
fn gate_pause(gate: &mut GateCoordinator, generation: u64) -> ffi::SubmitStatus {
    let status = Generation::new(generation)
        .map(|g| gate.submit(g, gate::PlaybackIntent::TogglePause))
        .unwrap_or(gate::SubmitStatus::StaleGeneration);
    match status {
        gate::SubmitStatus::Accepted => ffi::SubmitStatus::Accepted,
        gate::SubmitStatus::StaleGeneration => ffi::SubmitStatus::StaleGeneration,
        gate::SubmitStatus::NotReady => ffi::SubmitStatus::NotReady,
        gate::SubmitStatus::Closing => ffi::SubmitStatus::Closing,
        gate::SubmitStatus::CapacityExceeded => ffi::SubmitStatus::CapacityExceeded,
    }
}
fn gate_close(gate: &mut GateCoordinator, generation: u64, application: bool) -> ffi::UiUpdate {
    let target = if application {
        gate::CloseTarget::Application
    } else {
        gate::CloseTarget::Session
    };
    Generation::new(generation)
        .map(|g| gate.close(g, target))
        .unwrap_or_else(|| gate.unchanged())
        .into()
}
fn gate_quit(gate: &mut GateCoordinator) -> ffi::UiUpdate {
    gate.request_quit().into()
}
fn gate_native_released(gate: &mut GateCoordinator, generation: u64) -> ffi::UiUpdate {
    Generation::new(generation)
        .map(|g| gate.native_released(g))
        .unwrap_or_else(|| gate.unchanged())
        .into()
}
fn gate_poll(gate: &mut GateCoordinator) -> ffi::UiUpdate {
    gate.poll().into()
}
fn display_check(platform: &str, captured: &str, qt_display: &str, current: &str) -> String {
    crate::native_host::display_check(platform, captured, qt_display, current)
}
