//! GUI-thread GPUI adapter. Only the runtime can authorize native retirement.
mod brume;
mod capture_context;
mod chrome;
mod companion_view;
mod controls;
mod decisions;
mod device_settings;
mod hyprland_blur;
mod idle_inhibitor;
mod log_folder;
mod playback_menu;
mod preferences_page;
mod renderer_page;
mod settings_menu;
mod text_field;
mod top_chrome;
mod volume;
mod x11;

use std::{
    cell::RefCell,
    ffi::{CStr, c_char, c_int},
    io::{self, BufRead},
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    sync::mpsc::{Receiver, TryRecvError, sync_channel},
    time::{Duration, Instant},
};

use gpui::{
    App, BackgroundExecutor, Bounds, Context, FocusHandle, GraphicalEnvironment, KeyDownEvent,
    KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Render, ScrollEvent, Subscription,
    TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowDecorations,
    WindowHandle, WindowKind, WindowOptions, div, point, prelude::*, px, size,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{
    app::gate::GatePhase,
    domain::state::AttemptId,
    media::controller::{Generation, SurfaceToken, X11WindowId},
    ui::{
        projection::{SessionKind, Snapshot},
        runtime::{
            QualificationCommand, RuntimeCoordinator, UiUpdate, log_qualification_rejection,
        },
    },
};

use self::x11::{DestroyOutcome, Event, Layout, X11};
use self::{
    chrome::ChromeState,
    companion_view::{CompanionPaint, CompanionView},
    hyprland_blur::BlurOutcome,
    x11::{CompanionFacts, CompanionGeometry, FocusTarget},
};

const CHROME_APP_ID: &str = "dev.antho.furami.chrome";
const TICK: Duration = Duration::from_millis(16);
const COMPANION_PREPARE_TIMEOUT: Duration = Duration::from_secs(2);
const FULLSCREEN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum GpuiLaunchError {
    #[error("could not initialize the GPUI X11 window")]
    Startup,
    #[error("GPUI lost its native window or event loop")]
    NativeLoss,
    #[error("video qualification ended with a media failure")]
    QualificationFailure,
}

impl GpuiLaunchError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Startup => "gpui_native_startup",
            Self::NativeLoss => "gpui_native_loss",
            Self::QualificationFailure => "qualification",
        }
    }
}

fn can_reserve(phase: GatePhase, attempt: AttemptId, last_attempt: Option<AttemptId>) -> bool {
    phase == GatePhase::WaitingSurface && last_attempt.is_none_or(|last| last.get() < attempt.get())
}

struct Lease {
    attempt: AttemptId,
    xid: Option<X11WindowId>,
    layout: Option<Layout>,
    mapped: bool,
    observed_mapped: bool,
    prepared_paused: bool,
    poisoned: bool,
    absent: bool,
}

impl Lease {
    fn matches_release(&self, phase: GatePhase, attempt: AttemptId) -> bool {
        phase == GatePhase::Releasing && self.attempt == attempt
    }

    fn wants_mapped(&self, view: &ViewState) -> bool {
        !self.prepared_paused && !view.disconnected && view.close_dialog.is_empty()
    }
}

struct ViewState {
    product_opening: bool,
    phase: GatePhase,
    attempt: Option<AttemptId>,
    generation: u64,
    restart_generation: u64,
    snapshot: Option<Rc<Snapshot>>,
    can_open: bool,
    can_restart: bool,
    can_toggle_pause: bool,
    paused: bool,
    disconnected: bool,
    failed: bool,
    diagnostic: String,
    decision_diagnostic: String,
    settings_status: String,
    stats_hidden: bool,
    logging_enabled: bool,
    close_dialog: String,
    fullscreen: bool,
    closing: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            product_opening: false,
            phase: GatePhase::Idle,
            attempt: None,
            generation: 0,
            restart_generation: 0,
            snapshot: None,
            can_open: false,
            can_restart: false,
            can_toggle_pause: false,
            paused: false,
            disconnected: false,
            failed: false,
            diagnostic: String::new(),
            decision_diagnostic: String::new(),
            settings_status: String::new(),
            close_dialog: String::new(),
            stats_hidden: false,
            logging_enabled: false,
            fullscreen: false,
            closing: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Open,
    TogglePause(AttemptId, bool),
}

impl ViewState {
    fn action(&self) -> Option<Action> {
        if self.closing
            || !self.close_dialog.is_empty()
            || self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.opening)
        {
            return None;
        }
        if self.can_toggle_pause
            && let Some(attempt) = self.attempt
        {
            return Some(Action::TogglePause(attempt, self.paused));
        }
        if self.can_open
            && self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| !snapshot.can_reconnect)
        {
            return Some(Action::Open);
        }
        None
    }
}

struct PendingFullscreen {
    desired: bool,
    persist: bool,
    deadline: Instant,
}

impl PendingFullscreen {
    fn new(desired: bool, persist: bool, now: Instant) -> Self {
        Self {
            desired,
            persist,
            deadline: now + FULLSCREEN_TIMEOUT,
        }
    }

    // None = still pending; true = confirmed user request; false = retire
    // without persistence (expired, or restoring an existing preference).
    fn observe(&self, actual: bool, now: Instant) -> Option<bool> {
        if now >= self.deadline {
            Some(false)
        } else {
            (actual == self.desired).then_some(self.persist)
        }
    }
}

struct HostState {
    runtime: RuntimeCoordinator,
    display: Box<str>,
    x11: Option<X11>,
    root: u32,
    companion: Option<WindowHandle<CompanionView>>,
    companion_paint: Rc<RefCell<Option<CompanionPaint>>>,
    companion_input_popup: Option<chrome::Popup>,
    remap_popup_focus: Option<chrome::Popup>,
    keyboard_popup_focus: Option<chrome::Popup>,
    companion_mode: CompanionMode,
    companion_blur: Option<CompanionBlur>,
    companion_geometry: Option<CompanionGeometry>,
    companion_facts: Option<CompanionFacts>,
    chrome: ChromeState,
    volume: volume::VolumeState,
    top_refresh_due: Option<Instant>,
    settings_refresh_due: Option<Instant>,
    log_folder_pending: Option<(u64, String)>,
    qualification_input: Option<Receiver<QualificationCommand>>,
    overlay_opacity: f32,
    overlay_epoch: u64,
    focus_intent: bool,
    return_focus_intent: bool,
    keyboard_intent: Option<(bool, Instant)>,
    fullscreen_intent: bool,
    keyboard_time: Option<(FocusTarget, u32, Instant)>,
    native_fullscreen: bool,
    lease: Option<Lease>,
    last_attempt: Option<AttemptId>,
    last_retired_xid: u32,
    view: ViewState,
    layout: Option<Layout>,
    bounds_changed: bool,
    fullscreen_restored: bool,
    fullscreen_pending: Option<PendingFullscreen>,
    f11_down: bool,
    scale: f32,
    redraw: bool,
    started: bool,
    closing: bool,
    root_lost: bool,
    control_lost: bool,
    quit_authorized: bool,
    quit_requested: bool,
    uncontrolled_exit: bool,
    failure: Option<GpuiLaunchError>,
    qualification_failure: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompanionMode {
    Unmapped,
    AwaitingBlur(u64),
    Preparing(Instant),
    Visible,
    Lost,
}

#[derive(Default)]
struct CompanionBlur {
    blur_map_serial: u64,
    outcome: Option<BlurOutcome>,
    pending: Option<Receiver<(u64, BlurOutcome)>>,
}

impl CompanionBlur {
    fn start(&mut self, background: &BackgroundExecutor) -> CompanionMode {
        debug_assert!(self.pending.is_none() && self.outcome.is_none());
        self.blur_map_serial += 1;
        let serial = self.blur_map_serial;
        let (tx, rx) = sync_channel(1);
        self.pending = Some(rx);
        background
            .spawn(async move {
                let outcome = hyprland_blur::apply().await;
                let _ = tx.try_send((serial, outcome));
            })
            .detach();
        CompanionMode::AwaitingBlur(serial)
    }

    fn poll(&mut self, mode: CompanionMode) -> Option<BlurOutcome> {
        let (serial, outcome) = match self.pending.as_ref()?.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => (self.blur_map_serial, BlurOutcome::Unavailable),
        };
        self.pending = None;
        // A hide or native demap ends the current phase but leaves the
        // receiver in flight; only drain it, never settle a stale result.
        (mode == CompanionMode::AwaitingBlur(serial)).then_some(outcome)
    }
}

enum FocusChange {
    ToChrome { backward: bool },
    ToVideo,
}

impl HostState {
    fn new(runtime: RuntimeCoordinator, x11: Option<X11>, display: &str) -> Self {
        Self {
            runtime,
            display: display.into(),
            x11,
            root: 0,
            companion: None,
            companion_paint: Rc::default(),
            companion_input_popup: None,
            remap_popup_focus: None,
            keyboard_popup_focus: None,
            companion_mode: CompanionMode::Unmapped,
            companion_blur: None,
            companion_geometry: None,
            companion_facts: None,
            chrome: ChromeState::default(),
            volume: volume::VolumeState::default(),
            top_refresh_due: None,
            settings_refresh_due: None,
            log_folder_pending: None,
            overlay_opacity: 0.,
            overlay_epoch: 0,
            focus_intent: false,
            return_focus_intent: false,
            keyboard_intent: None,
            fullscreen_intent: false,
            keyboard_time: None,
            native_fullscreen: false,
            lease: None,
            last_attempt: None,
            last_retired_xid: 0,
            view: ViewState::default(),
            layout: None,
            bounds_changed: false,
            fullscreen_restored: false,
            fullscreen_pending: None,
            f11_down: false,
            scale: 1.,
            redraw: true,
            started: false,
            closing: false,
            root_lost: false,
            control_lost: false,
            quit_authorized: false,
            quit_requested: false,
            uncontrolled_exit: false,
            failure: None,
            qualification_failure: false,
            qualification_input: None,
        }
    }

    fn xid(&self) -> Option<u32> {
        self.lease.as_ref()?.xid.map(X11WindowId::get)
    }

    fn x11(&self) -> &X11 {
        self.x11.as_ref().expect("X11 connection before retirement")
    }

    fn fail_stop(&self, code: &'static str) -> ! {
        tracing::error!(
            event_code = "gpui_exit",
            code,
            generation = self.log_generation(),
            root = self.root,
            host = self.xid().unwrap_or(self.last_retired_xid),
            phase = phase_name(self.view.phase),
            exit_code = 1,
            authorized = false,
        );
        eprintln!("furami: {code}");
        // Dropping the X11 connection destroys its windows. Never unwind
        // past an unacknowledged owner barrier or claim a successful release.
        std::process::abort();
    }

    fn log_generation(&self) -> u64 {
        self.lease
            .as_ref()
            .map(|lease| lease.attempt)
            .or(self.view.attempt)
            .map_or(0, AttemptId::get)
    }

    fn check_barrier(&self) {
        if self.runtime.owner_ack_failed() {
            self.fail_stop("gpui_owner_barrier_failed");
        }
    }

    fn consume(&mut self, update: UiUpdate) {
        let UiUpdate {
            changed,
            ui_snapshot,
            phase,
            generation,
            restart_generation,
            product_phase,
            failed,
            can_open,
            can_restart,
            can_toggle_pause,
            paused,
            context_diagnostic,
            diagnostic,
            settings_status,
            close_dialog,
            stats_hidden,
            logging_enabled,
            fullscreen,
            closing,
            prepared_paused,
            terminal_failure,
            create_native,
            release_native,
            quit,
            log_folder_request,
            log_folder_path,
            ..
        } = update;
        let attempt = AttemptId::new(generation);
        self.check_barrier();
        self.qualification_failure |= terminal_failure;
        if let Some(snapshot) = ui_snapshot {
            self.view.snapshot = Some(snapshot);
            self.redraw = true;
        }
        if changed {
            if self.view.phase != phase || self.view.paused != paused {
                self.volume.reset_wheel();
            }
            self.view.phase = phase;
            self.view.generation = generation;
            self.view.restart_generation = restart_generation;
            // Qualification projects its constant generation even when idle.
            self.view.attempt = attempt.filter(|_| {
                create_native
                    || self.lease.is_some()
                    || matches!(
                        phase,
                        GatePhase::WaitingSurface | GatePhase::Opening | GatePhase::Ready
                    )
            });
            self.view.disconnected = product_phase == "Disconnected";
            self.view.product_opening =
                matches!(phase, GatePhase::WaitingSurface | GatePhase::Opening)
                    || self
                        .view
                        .snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.opening);
            self.view.failed = failed;
            self.view.can_open = can_open;
            self.view.can_restart = can_restart;
            self.view.can_toggle_pause = can_toggle_pause;
            self.view.paused = paused;
            self.view.diagnostic = context_diagnostic;
            self.view.decision_diagnostic = diagnostic;
            self.view.settings_status = settings_status;
            self.view.close_dialog = close_dialog;
            self.view.stats_hidden = stats_hidden;
            self.view.logging_enabled = logging_enabled;
            self.view.closing = closing;
            self.redraw = true;
        }
        if (changed || matches!(self.runtime, RuntimeCoordinator::Qualification(_)))
            && self.view.fullscreen != fullscreen
        {
            self.view.fullscreen = fullscreen;
            self.redraw = true;
        }
        if let Some(lease) = &mut self.lease
            && Some(lease.attempt) == attempt
        {
            lease.prepared_paused |= prepared_paused;
        }

        if log_folder_request != 0 {
            self.log_folder_pending = Some((log_folder_request, log_folder_path));
        }

        // These are delivery edges, independent of scalar/snapshot equality.
        if create_native
            && let Some(attempt) = attempt
            && can_reserve(phase, attempt, self.last_attempt)
            && self.lease.is_none()
        {
            self.create(attempt, prepared_paused);
        }
        if release_native
            && let Some(attempt) = attempt
            && self
                .lease
                .as_ref()
                .is_some_and(|lease| lease.matches_release(phase, attempt))
        {
            self.release(attempt);
        }
        if quit && self.lease.is_none() {
            self.quit_authorized = true;
        }
    }

    fn create(&mut self, attempt: AttemptId, prepared_paused: bool) {
        self.last_attempt = Some(attempt);
        // Reserve also on layout/creation failure: a partially successful X11
        // request still needs a real owner barrier and native release edge.
        self.lease = Some(Lease {
            attempt,
            xid: None,
            layout: None,
            mapped: false,
            observed_mapped: false,
            prepared_paused,
            poisoned: false,
            absent: false,
        });
        if self.root_lost || self.failure.is_some() {
            self.report_loss("publication-blocked", false, self.root_lost);
            return;
        }
        match self.create_surface() {
            Ok(token) => {
                let update = self.runtime.surface_ready(token);
                tracing::info!(
                    event_code = "gpui_publish",
                    generation = attempt.get(),
                    root = self.root,
                    host = token.xid.get(),
                    phase = phase_name(update.phase),
                    mapped = self.lease.as_ref().is_some_and(|lease| lease.mapped),
                );
                self.consume(update);
            }
            Err(error) => self.report_x11_error("create-failed", &error),
        }
    }

    fn create_surface(&mut self) -> Result<SurfaceToken, x11::Error> {
        let extent = self.x11().extent()?;
        let layout = Layout::new(extent, self.scale)?
            .ok_or(x11::Error::Geometry("empty initial video geometry"))?;
        let xid = self.x11().allocate()?;
        let lease = self.lease.as_mut().expect("reserved lease");
        lease.xid = Some(X11WindowId::new(u64::from(xid)).expect("allocated XID is nonzero"));
        let token = SurfaceToken {
            generation: Generation::new(lease.attempt.get()).expect("attempt is nonzero"),
            xid: lease.xid.expect("allocated XID"),
        };
        if let Err(error) = self.x11().create(xid, extent) {
            if error.is_rejected() {
                self.lease.as_mut().expect("reserved lease").xid = None;
            }
            return Err(error);
        }
        self.x11().apply_layout(xid, layout)?;
        self.lease.as_mut().expect("reserved lease").layout = Some(layout);
        self.layout = Some(layout);
        self.redraw = true;
        self.update_mapping()?;
        self.x11().verify_parent(xid)?;
        self.log_layout();
        Ok(token)
    }

    fn update_mapping(&mut self) -> Result<(), x11::Error> {
        if let Some(lease) = &self.lease
            && !lease.poisoned
            && let Some(xid) = lease.xid
        {
            let mapped = lease.wants_mapped(&self.view);
            if lease.mapped != mapped {
                self.x11().set_mapped(xid.get(), mapped)?;
                self.lease.as_mut().expect("same lease").mapped = mapped;
                self.redraw = true;
            }
        }
        Ok(())
    }

    fn resize(&mut self, geometry_changed: bool, scale: f32) -> Result<(), x11::Error> {
        if !geometry_changed && self.scale == scale {
            return Ok(());
        }
        if self.scale != scale {
            self.x11().set_minimum_scale(scale)?;
        }
        let extent = if geometry_changed {
            self.x11().extent()?
        } else if let Some(layout) = self.layout {
            layout.extent
        } else {
            return Ok(());
        };
        let Some(layout) = Layout::new(extent, scale)? else {
            return Ok(());
        };
        let mut applied = false;
        if let Some(lease) = &mut self.lease
            && !lease.poisoned
            && let Some(xid) = lease.xid
            && lease.layout != Some(layout)
        {
            self.x11
                .as_ref()
                .expect("X11 connection before retirement")
                .apply_layout(xid.get(), layout)?;
            lease.layout = Some(layout);
            applied = true;
        }
        let changed = self.scale != scale || self.layout != Some(layout);
        self.scale = scale;
        self.layout = Some(layout);
        self.redraw |= changed;
        if applied || changed {
            self.log_layout();
        }
        Ok(())
    }

    fn log_layout(&self) {
        if let Some(lease) = &self.lease
            && let Some(layout) = lease.layout
        {
            tracing::info!(
                event_code = "gpui_layout",
                generation = lease.attempt.get(),
                root = self.root,
                host = self.xid().unwrap_or_default(),
                root_width = layout.extent.0,
                root_height = layout.extent.1,
                dpr = f64::from(self.scale),
                mapped = lease.mapped,
            );
        }
    }

    fn release(&mut self, attempt: AttemptId) {
        let lease = self.lease.as_ref().expect("matching release");
        let root = self.root;
        let host = self.xid().unwrap_or_default();
        let mut poisoned = lease.poisoned;
        let status = if self.control_lost {
            self.x11 = None;
            "ConnectionRetired"
        } else {
            match lease.xid {
                None => "NoXid",
                Some(_) if lease.absent => "ExternallyAbsent",
                Some(xid) => match self.x11().destroy(xid.get()) {
                    Ok(DestroyOutcome::Destroyed) => "Destroyed",
                    Ok(DestroyOutcome::Absent) => {
                        // Notification may be queued after this turn's event drain.
                        // The owner is already retired; fence any replacement now.
                        self.failure.get_or_insert(GpuiLaunchError::NativeLoss);
                        poisoned = true;
                        tracing::error!(
                            event_code = "gpui_surface_loss",
                            reason = "child-absent-at-release",
                            generation = attempt.get(),
                            root,
                            host,
                            phase = "Releasing",
                        );
                        "ExternallyAbsent"
                    }
                    Err(error) if error.is_connection_loss() => {
                        // Closing a broken control connection retires its resources,
                        // but proves neither a checked destroy nor loss of the GPUI window.
                        self.x11 = None;
                        self.control_lost = true;
                        self.failure.get_or_insert(GpuiLaunchError::NativeLoss);
                        "ConnectionRetired"
                    }
                    Err(_) => self.fail_stop("gpui_native_loss"),
                },
            }
        };
        tracing::info!(
            event_code = "gpui_release",
            generation = attempt.get(),
            root,
            host,
            phase = "Releasing",
            status,
            poisoned,
            authorized = true,
        );
        if host != 0 {
            self.last_retired_xid = host;
        }
        self.lease = None;
        self.redraw = true;
        let update = self.runtime.native_released(attempt);
        self.consume(update);
        if matches!(status, "ConnectionRetired" | "ExternallyAbsent")
            && !self.quit_authorized
            && !self.closing
        {
            if self.root_lost {
                self.force_close();
            } else {
                self.request_close();
            }
        }
    }

    fn report_x11_error(&mut self, reason: &'static str, error: &x11::Error) {
        if error.is_connection_loss() && !self.control_lost {
            self.control_lost = true;
            if let Some(x11) = &self.x11
                && x11.companion_xid().is_some()
            {
                if let Err(error) = x11.quarantine_companion_after_connection_loss(&self.display) {
                    tracing::error!(event_code = "gpui_companion_quarantine_failed", %error);
                }
                self.companion_mode = CompanionMode::Lost;
                self.companion_blur = None;
                self.volume.reset();
                self.set_overlay_opacity(0.);
            }
        }
        if self.control_lost && self.lease.is_none() {
            self.x11 = None;
        }
        self.report_loss(reason, false, false);
    }

    fn report_loss(&mut self, reason: &'static str, absent: bool, root_lost: bool) {
        self.failure.get_or_insert(GpuiLaunchError::NativeLoss);
        self.root_lost |= root_lost;
        tracing::error!(
            event_code = "gpui_surface_loss",
            reason,
            generation = self.log_generation(),
            root = self.root,
            host = self.xid().unwrap_or_default(),
            phase = phase_name(self.view.phase),
        );
        if let Some(lease) = &mut self.lease {
            lease.absent |= absent;
            if !lease.poisoned {
                lease.poisoned = true;
                let attempt = lease.attempt;
                let update = self.runtime.surface_lost(attempt);
                self.consume(update);
                if self
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.attempt == attempt)
                {
                    if !self.runtime.wait_for_owner_ack(attempt).is_empty() {
                        self.fail_stop("gpui_owner_barrier_failed");
                    }
                    // A buffered receipt is not a release effect.
                    let update = self.runtime.poll();
                    self.consume(update);
                }
            }
        }
        if !self.quit_authorized {
            if self.root_lost {
                self.force_close();
            } else if !self.closing {
                self.request_close();
            }
        }
    }

    fn request_close(&mut self) {
        self.closing = true;
        self.cancel_fullscreen();
        let update = self.runtime.request_application_close();
        self.consume(update);
    }

    fn wm_close(&mut self) {
        tracing::info!(
            event_code = "gpui_wm_close",
            generation = self.log_generation(),
            root = self.root,
            host = self.xid().unwrap_or_default(),
            phase = phase_name(self.view.phase),
            closing = self.closing,
        );
        self.request_close();
    }

    fn companion_lost(&mut self, reason: &'static str) {
        if self.companion_mode == CompanionMode::Lost {
            return;
        }
        tracing::error!(event_code = "gpui_companion_loss", reason, root = self.root);
        self.companion_mode = CompanionMode::Lost;
        self.companion_blur = None;
        self.volume.reset();
        self.overlay_opacity = 0.;
        self.failure.get_or_insert(GpuiLaunchError::NativeLoss);
        // The managed root can still display save/durability decisions. The
        // loss of chrome is neither a lost media surface nor an owner ACK.
        if !self.quit_authorized {
            self.request_close();
        }
    }

    fn set_overlay_opacity(&mut self, opacity: f32) {
        if self.overlay_opacity != opacity {
            self.overlay_opacity = opacity;
            self.redraw = true;
        }
    }

    fn hide_companion(&mut self) -> Result<(), x11::Error> {
        if matches!(
            self.companion_mode,
            CompanionMode::Visible | CompanionMode::Preparing(_) | CompanionMode::AwaitingBlur(_)
        ) {
            let focus_result = self.x11().return_focus();
            self.x11().hide_companion()?;
            self.companion_mode = CompanionMode::Unmapped;
            self.companion_input_popup = None;
            self.set_overlay_opacity(0.);
            self.volume.reset();
            // Unmapping can end the pointer grab before a release reaches chrome.
            self.chrome.press(false, Instant::now());
            tracing::info!(
                event_code = "gpui_companion_hidden",
                epoch = self.overlay_epoch
            );
            focus_result?;
        }
        Ok(())
    }

    fn stage_companion(&mut self, now: Instant) {
        self.overlay_epoch += 1;
        self.companion_mode = CompanionMode::Preparing(now);
        self.redraw = true;
        tracing::info!(
            event_code = "gpui_companion_stage",
            xid = self.x11().companion_xid(),
            epoch = self.overlay_epoch,
        );
    }

    fn reconcile_companion(
        &mut self,
        background: &BackgroundExecutor,
        native_changed: bool,
        geometry_changed: bool,
        companion_changed: bool,
        blur_interrupted: bool,
    ) -> Result<Option<FocusChange>, x11::Error> {
        if self.companion.is_none() || self.companion_mode == CompanionMode::Lost {
            return Ok(None);
        }
        let awaiting_blur = matches!(self.companion_mode, CompanionMode::AwaitingBlur(_));
        if awaiting_blur && blur_interrupted {
            // Final facts alone can hide an Unmap→Map or focus loss and
            // regain. Retire this phase before polling the reply and leave
            // another tick to drain the unmap we are about to issue.
            self.hide_companion()?;
            self.companion_facts = Some(self.x11().companion_facts()?);
            return Ok(None);
        }
        let blur_outcome = self
            .companion_blur
            .as_mut()
            .and_then(|blur| blur.poll(self.companion_mode));
        if native_changed || self.companion_facts.is_none() || blur_outcome.is_some() {
            self.companion_facts = Some(self.x11().companion_facts()?);
        }
        let available = self.accepts_input() && self.owned_family_viewable();
        let now = Instant::now();
        self.chrome
            .reconcile(available, self.view.snapshot.as_deref(), now);
        if available
            && matches!(
                self.decision_view().identity(),
                Some(decisions::Identity::Reset(_))
            )
        {
            if !self
                .chrome
                .popup()
                .is_some_and(|popup| popup.invoker == decisions::RESET_POPUP)
            {
                self.chrome.open_popup(decisions::RESET_POPUP, now);
                self.keyboard_popup_focus = self.chrome.popup();
                self.redraw = true;
            }
        } else if let Some(popup) = self.chrome.popup()
            && popup.invoker == decisions::RESET_POPUP
        {
            self.chrome.dismiss(popup, now);
            self.return_focus_intent = true;
            self.redraw = true;
        }
        if !available {
            self.volume.reset();
        } else if !self.volume_pointer_available() {
            self.volume.cancel_drag();
            self.volume.reset_wheel();
        }
        if self.keyboard_popup_focus != self.chrome.popup() {
            self.keyboard_popup_focus = None;
        }
        let resizable_popup = self
            .chrome
            .popup()
            .filter(|popup| popup.invoker != controls::OPTIONS_ID);
        if self.remap_popup_focus != resizable_popup {
            self.remap_popup_focus = None;
        }
        let opacity = self.chrome.opacity(now);
        if opacity == 0. {
            self.hide_companion()?;
            if !available {
                self.focus_intent = false;
                self.return_focus_intent = false;
                self.keyboard_intent = None;
                self.keyboard_time = None;
            }
            return Ok(None);
        }
        if (native_changed || blur_outcome.is_some())
            && matches!(
                self.companion_mode,
                CompanionMode::Visible | CompanionMode::Preparing(_) | CompanionMode::AwaitingBlur(_)
            )
            && self
                .companion_facts
                .as_ref()
                .is_some_and(|facts| !facts.companion_viewable)
        {
            self.hide_companion()?;
            self.companion_geometry = None;
            if awaiting_blur {
                return Ok(None);
            }
        }
        if matches!(self.companion_mode, CompanionMode::AwaitingBlur(_)) {
            if let Some(outcome) = blur_outcome {
                let geometry = match outcome {
                    BlurOutcome::Applied => Some(self.x11().companion_geometry()?),
                    BlurOutcome::Unavailable => None,
                };
                if self.x11().blur_reply_interrupted(self.xid())? {
                    // The normal drain will handle the queued interruption
                    // before this phase can accept another reply.
                    return Ok(None);
                }
                if let Some(geometry) = geometry {
                    // Still mapped and in the focused family. Resize this XID
                    // in place: the invalidated 1×1 paint is intentional.
                    self.x11().place_companion(geometry)?;
                    self.companion_geometry = Some(geometry);
                    if let Some(blur) = &mut self.companion_blur {
                        blur.outcome = Some(outcome);
                    }
                    tracing::info!(event_code = "gpui_hyprland_blur_applied");
                    self.stage_companion(now);
                } else {
                    self.hide_companion()?;
                    if let Some(blur) = &mut self.companion_blur {
                        blur.outcome = Some(outcome);
                    }
                    tracing::warn!(event_code = "gpui_hyprland_blur_unavailable");
                }
            }
            return Ok(None);
        }
        if self.companion_mode == CompanionMode::Unmapped
            && let Some(blur) = &self.companion_blur
            && blur.outcome.is_none()
        {
            if blur.pending.is_none() {
                self.companion_geometry = None;
                self.x11()
                    .place_companion(CompanionGeometry::HYPRLAND_BOOTSTRAP)?;
                if self.x11().map_hyprland_bootstrap()? {
                    self.companion_mode = self
                        .companion_blur
                        .as_mut()
                        .expect("verified Hyprland companion")
                        .start(background);
                }
            }
            return Ok(None);
        }
        if geometry_changed || companion_changed || self.companion_geometry.is_none() {
            let geometry = self.x11().companion_geometry()?;
            let resized = self
                .companion_geometry
                .is_some_and(|old| (old.width, old.height) != (geometry.width, geometry.height));
            if resized {
                if self
                    .companion_facts
                    .as_ref()
                    .is_some_and(|facts| facts.focus == Some(FocusTarget::Companion))
                {
                    self.remap_popup_focus = resizable_popup;
                }
                self.hide_companion()?;
            }
            if self.x11().place_companion(geometry)? {
                self.hide_companion()?;
            }
            self.companion_geometry = Some(geometry);
        }
        let Some(geometry) = self.companion_geometry else {
            return Ok(None);
        };
        let extent = (geometry.width, geometry.height);
        if self.companion_mode == CompanionMode::Unmapped {
            if self.x11().map_companion()? {
                self.stage_companion(now);
            }
            return Ok(None);
        }
        // GPUI reports the scene committed by the frame loop; the native
        // compositor proof separately verifies what actually became visible.
        let paint = self.companion_paint.borrow_mut().take();
        let mut focus = None;
        if let Some(paint) = paint
            && paint.epoch == self.overlay_epoch
            && paint.extent == extent
            && paint.popup == self.chrome.popup()
            && (self.companion_mode == CompanionMode::Visible || paint.blank)
        {
            if !self
                .x11()
                .present_companion(self.scale, &paint.hits, paint.modal)?
            {
                self.hide_companion()?;
                self.companion_geometry = None;
                return Ok(None);
            }
            self.companion_input_popup = if !paint.blank && !paint.hits.is_empty() {
                paint.popup
            } else {
                None
            };
            // Restore only the popup that lost focus to this resize.
            // The checked transfer still requires the active, viewable family.
            if let Some(popup) = self.remap_popup_focus
                && self.companion_input_popup == Some(popup)
            {
                self.remap_popup_focus = None;
                self.x11().focus_companion()?;
            }
            if let Some(popup) = self.keyboard_popup_focus
                && self.companion_input_popup == Some(popup)
            {
                self.keyboard_popup_focus = None;
                if self.x11().focus_companion()? {
                    focus = Some(FocusChange::ToChrome { backward: false });
                }
            }
            if matches!(self.companion_mode, CompanionMode::Preparing(_)) {
                self.companion_mode = CompanionMode::Visible;
                tracing::info!(
                    event_code = "gpui_companion_visible",
                    xid = self.x11().companion_xid(),
                    epoch = self.overlay_epoch,
                );
            }
        }
        if let CompanionMode::Preparing(since) = self.companion_mode
            && now.duration_since(since) >= COMPANION_PREPARE_TIMEOUT
        {
            self.hide_companion()?;
            self.companion_lost("companion-preparation-timeout");
            return Ok(None);
        }
        if self.companion_mode == CompanionMode::Visible {
            self.set_overlay_opacity(opacity);
            let key = self
                .keyboard_time
                .filter(|(_, _, received)| now.saturating_duration_since(*received) <= TICK * 6);
            self.keyboard_time = key;
            if std::mem::take(&mut self.return_focus_intent) {
                let returned = if let Some((FocusTarget::Companion, time, _)) = key {
                    self.keyboard_time = None;
                    self.x11().focus_with_key(FocusTarget::Root, time)?
                        || self.x11().return_focus()?
                } else {
                    self.x11().return_focus()?
                };
                if returned {
                    focus = Some(FocusChange::ToVideo);
                }
            } else if std::mem::take(&mut self.focus_intent) {
                self.x11().focus_companion()?;
            } else if let Some((backward, _)) = self
                .keyboard_intent
                .filter(|(_, requested)| now.saturating_duration_since(*requested) <= TICK * 6)
                && let Some((FocusTarget::Root, time, _)) = key
            {
                self.keyboard_intent = None;
                self.keyboard_time = None;
                if self.x11().focus_with_key(FocusTarget::Companion, time)? {
                    focus = Some(FocusChange::ToChrome { backward });
                }
            }
        }
        Ok(focus)
    }

    fn force_close(&mut self) {
        self.closing = true;
        self.cancel_fullscreen();
        let update = self.runtime.quit();
        self.consume(update);
    }

    fn drain_without_gui(&mut self, reason: &'static str) {
        if self.started {
            if !self.uncontrolled_exit {
                self.uncontrolled_exit = true;
                self.report_loss(reason, false, true);
            }
        } else {
            self.failure.get_or_insert(GpuiLaunchError::Startup);
            self.root_lost = true;
            if !self.quit_authorized {
                self.force_close();
            }
        }
        while !self.quit_authorized {
            let update = self.runtime.poll();
            self.consume(update);
            if !self.quit_authorized {
                std::thread::sleep(TICK);
            }
        }
    }

    fn cancel_fullscreen(&mut self) {
        self.f11_down = false;
        self.fullscreen_pending = None;
        self.fullscreen_intent = false;
    }

    fn accepts_input(&self) -> bool {
        !self.closing
            && !self.view.closing
            && self.failure.is_none()
            && !self.qualification_failure
            && !self.quit_authorized
    }

    fn owned_family_viewable(&self) -> bool {
        self.companion_facts
            .is_some_and(|facts| facts.root_viewable && facts.focus.is_some())
    }

    fn wheel_family_viewable(&self, companion: bool) -> bool {
        // X11 delivers wheel events under the pointer even after keyboard focus
        // leaves Furami. The last render poll cannot authorize this event.
        self.volume_pointer_available()
            && (!companion || self.companion_mode == CompanionMode::Visible)
            && self.x11.as_ref().is_some_and(|x11| {
                x11.companion_facts().is_ok_and(|facts| {
                    facts.root_viewable
                        && facts.focus.is_some()
                        && (!companion || facts.companion_viewable)
                })
            })
    }

    fn video_mapped(&self) -> bool {
        self.lease.as_ref().is_some_and(|lease| lease.mapped)
    }

    fn idle_inhibit_wanted(&self) -> bool {
        !self.root_lost
            && !self.control_lost
            && self.accepts_input()
            && self.view.phase == GatePhase::Ready
            && !self.view.paused
            && self.companion_facts.is_some_and(|facts| facts.root_mapped)
            && self
                .view
                .snapshot
                .as_deref()
                .is_some_and(|snapshot| !snapshot.paused && snapshot.live.playback_status == "Live")
            && self.lease.as_ref().is_some_and(|lease| {
                self.view.attempt == Some(lease.attempt)
                    && lease.xid.is_some()
                    && lease.layout.is_some()
                    && lease.mapped
                    && lease.observed_mapped
                    && !lease.poisoned
                    && !lease.absent
            })
    }

    fn decision_view(&self) -> decisions::DecisionView<'_> {
        decisions::DecisionView {
            close_dialog: &self.view.close_dialog,
            settings: self
                .view
                .snapshot
                .as_deref()
                .map(|snapshot| &snapshot.settings),
            status: &self.view.settings_status,
            diagnostic: &self.view.decision_diagnostic,
        }
    }

    fn dispatch_decision(
        &mut self,
        rendered: decisions::Identity,
        intent: decisions::Intent,
    ) -> bool {
        if self.decision_view().identity() != Some(rendered) || !rendered.accepts(intent) {
            return false;
        }
        let update = match intent {
            decisions::Intent::DecideReset { token, confirmed } if self.accepts_input() => {
                self.runtime.decide_reset(token.get(), confirmed)
            }
            decisions::Intent::DecideReset { .. } => return false,
            decisions::Intent::RetrySave => self.runtime.retry_save(),
            decisions::Intent::CloseWithoutSave => self.runtime.close_without_save(),
        };
        self.consume(update);
        true
    }

    fn action(&self) -> Option<Action> {
        if !self.accepts_input() {
            return None;
        }
        self.view
            .action()
            .filter(|action| *action != Action::Open || !self.video_mapped())
    }

    fn activate(&mut self, rendered: Action) {
        if self.action() != Some(rendered) {
            return;
        }
        let update = match rendered {
            Action::Open => self.runtime.open(),
            Action::TogglePause(attempt, _) => {
                self.runtime.pause(attempt);
                self.runtime.poll()
            }
        };
        self.consume(update);
    }

    fn f11_press(&mut self, actual: bool) -> bool {
        if !self.accepts_input() || std::mem::replace(&mut self.f11_down, true) {
            return false;
        }
        if !self.fullscreen_restored || self.fullscreen_pending.is_some() {
            return false;
        }
        self.fullscreen_pending = Some(PendingFullscreen::new(!actual, true, Instant::now()));
        true
    }

    /// Release the F11 latch only when the server proves the key is up.
    ///
    /// The backend can report a key-up while the physical key is still held;
    /// clearing the latch then would let the next key-down re-arm and toggle
    /// fullscreen a second time. A failed query keeps the latch, exactly like
    /// a still-held key, and follows the ordinary X11 error path.
    fn f11_release(&mut self) {
        if !self.f11_down {
            return;
        }
        if self.x11.is_none() || self.root_lost || self.control_lost {
            self.f11_down = false;
            return;
        }
        match self.x11().f11_down() {
            Ok(false) => self.f11_down = false,
            Ok(true) => {}
            Err(error) => self.report_x11_error("f11-state-failed", &error),
        }
    }

    fn request_fullscreen(&mut self) {
        self.fullscreen_intent |= self.f11_press(self.native_fullscreen);
    }

    fn poll_qualification_input(&mut self) {
        let Some(input) = &self.qualification_input else {
            return;
        };
        // Native observation runs before this poll and never waits for stdin.
        // Until startup restoration or an in-flight toggle settles, keep the
        // FIFO intact. WM close bypasses stdin and cancels the transition.
        if self.accepts_input() && (!self.fullscreen_restored || self.fullscreen_pending.is_some())
        {
            return;
        }
        match input.try_recv() {
            Ok(QualificationCommand::Fullscreen) => {
                if self.accepts_input() {
                    self.fullscreen_pending = Some(PendingFullscreen::new(
                        !self.native_fullscreen,
                        true,
                        Instant::now(),
                    ));
                    self.fullscreen_intent = true;
                }
            }
            Ok(command) => {
                let update = self.runtime.qualification_command(command);
                if command == QualificationCommand::Snapshot {
                    self.log_layout();
                }
                self.consume(update);
            }
            Err(TryRecvError::Disconnected) => self.qualification_input = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Drain native events first, and return whether to issue the saved-mode
    /// toggle outside this borrow.
    fn tick(
        &mut self,
        background: &BackgroundExecutor,
        scale: f32,
        fullscreen: bool,
    ) -> (bool, Option<FocusChange>) {
        let mut geometry_changed =
            std::mem::take(&mut self.bounds_changed) || self.native_fullscreen != fullscreen;
        self.native_fullscreen = fullscreen;
        let mut native_changed = false;
        let mut companion_changed = false;
        let mut blur_interrupted = false;
        if !self.root_lost && !self.control_lost {
            loop {
                let event = match self.x11().next_event(self.xid()) {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(error) => {
                        self.report_x11_error("x11-event-failed", &error);
                        break;
                    }
                };
                blur_interrupted |= event.interrupts_blur();
                match event {
                    Event::GeometryChanged | Event::ParentMappingChanged(_) => {
                        geometry_changed = true;
                    }
                    Event::CompanionChanged | Event::CompanionMappingChanged(_) => {
                        companion_changed = true;
                    }
                    Event::FocusChanged | Event::OwnedFocusDeparted => native_changed = true,
                    Event::ChildMappingChanged(mapped) => {
                        if let Some(lease) = &mut self.lease {
                            lease.observed_mapped = mapped;
                        }
                    }
                    Event::CompanionLost => {
                        let _ = self.x11().hide_companion();
                        self.companion_lost("companion-destroyed-or-reparented");
                    }
                    Event::KeyPress {
                        target,
                        time,
                        tab: true,
                    } => {
                        self.keyboard_time = Some((target, time, Instant::now()));
                    }
                    Event::KeyPress { .. } | Event::KeyRelease { .. } => {}
                    Event::ParentDestroyed => {
                        self.report_loss("root-destroyed", false, true);
                        break;
                    }
                    Event::ChildDestroyed => {
                        self.report_loss("child-destroyed", true, false)
                    }
                    Event::ChildReparented => {
                        self.report_loss("child-reparented", false, false)
                    }
                }
                if self.root_lost || self.control_lost || self.quit_authorized {
                    break;
                }
            }
        }
        if !self.root_lost
            && !self.control_lost
            && !self.quit_authorized
            && let Err(error) = self.resize(geometry_changed, scale)
        {
            self.report_x11_error("layout-failed", &error);
        }
        if !self.quit_authorized {
            let update = self.runtime.poll();
            self.consume(update);
        }
        if self.root_lost || self.control_lost {
            return (false, None);
        }
        let focus = match self.reconcile_companion(
            background,
            native_changed || companion_changed || geometry_changed,
            geometry_changed,
            companion_changed,
            blur_interrupted,
        ) {
            Ok(focus) => focus,
            Err(error) => {
                tracing::error!(event_code = "gpui_companion_native_failed", %error);
                if error.is_connection_loss() {
                    self.report_x11_error("companion-connection-failed", &error);
                } else {
                    let _ = self.hide_companion();
                    self.companion_lost("companion-native-failed");
                }
                return (false, None);
            }
        };
        if self.quit_authorized {
            return (false, None);
        }
        self.top_refresh_tick();
        self.tick_settings_refresh(Instant::now());
        if let Err(error) = self.update_mapping() {
            self.report_x11_error("mapping-failed", &error);
            return (false, None);
        }
        if !self.accepts_input() {
            self.cancel_fullscreen();
            return (false, None);
        }
        if let Some(persist) = self
            .fullscreen_pending
            .as_ref()
            .and_then(|pending| pending.observe(fullscreen, Instant::now()))
        {
            self.fullscreen_pending = None;
            if persist {
                // This observation, not the EWMH request or a preference,
                // authorizes persistence. Refresh server geometry first.
                if let Err(error) = self.resize(!geometry_changed, scale) {
                    self.report_x11_error("fullscreen-layout-failed", &error);
                    return (false, None);
                }
                let update = self.runtime.set_fullscreen(fullscreen);
                self.consume(update);
            }
        }
        if self.started && !self.fullscreen_restored && self.x11().parent_mapped() {
            self.fullscreen_restored = true;
            if self.view.fullscreen != fullscreen {
                self.fullscreen_pending = Some(PendingFullscreen::new(
                    self.view.fullscreen,
                    false,
                    Instant::now(),
                ));
                return (true, focus);
            }
        }
        (std::mem::take(&mut self.fullscreen_intent), focus)
    }
}

fn phase_name(phase: GatePhase) -> &'static str {
    match phase {
        GatePhase::Idle => "Idle",
        GatePhase::WaitingSurface => "WaitingSurface",
        GatePhase::Opening => "Opening",
        GatePhase::Ready => "Ready",
        GatePhase::Stopping => "Stopping",
        GatePhase::Releasing => "Releasing",
        GatePhase::Failed => "Failed",
        GatePhase::QuitReady => "QuitReady",
    }
}

fn install_window_close<V: 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
    state: &Rc<RefCell<HostState>>,
) {
    let on_close = Rc::downgrade(state);
    window.on_window_should_close(cx, move |window, cx| {
        let on_close = on_close.clone();
        window.defer(cx, move |_, _| {
            if let Some(state) = on_close.upgrade() {
                state.borrow_mut().wm_close();
            }
        });
        false
    });
}

struct HostView {
    state: Rc<RefCell<HostState>>,
    focus: FocusHandle,
    decision_focus: decisions::DecisionFocus,
    decision_scroll: gpui::ScrollHandle,
    idle_inhibitor: idle_inhibitor::IdleInhibitor,
    _subscriptions: [Subscription; 2],
}

impl HostView {
    fn new(window: &mut Window, cx: &mut Context<Self>, state: Rc<RefCell<HostState>>) -> Self {
        let focus = cx.focus_handle();
        let idle_inhibitor = idle_inhibitor::IdleInhibitor::new(cx.background_executor().clone());
        focus.focus(window, cx);
        let bounds = cx.observe_window_bounds(window, |this, _, _| {
            this.state.borrow_mut().bounds_changed = true;
        });
        // A control that unmounts (or stops being a tab stop) takes its focus
        // handle with it, which leaves the window with no focused element.
        let focus_lost = cx.on_focus_lost(window, |this, window, cx| {
            this.restore_host_focus(window, cx);
        });
        install_window_close(window, cx, &state);
        let on_window_loss = Rc::downgrade(&state);
        let app = cx.to_async();
        let background = cx.background_executor().clone();
        cx.spawn_in(window, async move |view, cx| {
            loop {
                background.timer(TICK).await;
                let result = cx.update(|window, cx| {
                    let view = view.upgrade()?;
                    Some(view.update(cx, |view, cx| {
                        let (redraw, quit, toggle_fullscreen, companion, focus, log_folder) = {
                            let mut state = view.state.borrow_mut();
                            let (toggle, focus) = state.tick(
                                &background,
                                window.scale_factor(),
                                window.is_fullscreen(),
                            );
                            state.poll_qualification_input();
                            let quit = state.quit_authorized && !state.quit_requested;
                            state.quit_requested |= quit;
                            (
                                std::mem::take(&mut state.redraw),
                                quit,
                                toggle,
                                state.companion,
                                focus,
                                state.log_folder_pending.take().filter(|(request, _)| {
                                    state.runtime.claim_log_folder_dispatch(*request)
                                }),
                            )
                        };
                        view.idle_inhibitor
                            .reconcile(view.state.borrow().idle_inhibit_wanted());
                        if let Some((request, path)) = log_folder {
                            log_folder::launch(cx, request, path);
                        }
                        if toggle_fullscreen {
                            window.toggle_fullscreen();
                        }
                        if let Some(companion) = companion
                            && (redraw || focus.is_some())
                            && !quit
                            && companion
                                .update(cx, |view, window, cx| {
                                    match focus {
                                        Some(FocusChange::ToChrome { backward }) => {
                                            view.request_focus(backward, window, cx)
                                        }
                                        Some(FocusChange::ToVideo) => {
                                            view.confirm_video_focus(window, cx)
                                        }
                                        None => {}
                                    }
                                    cx.notify();
                                })
                                .is_err()
                        {
                            view.state
                                .borrow_mut()
                                .companion_lost("companion-unavailable");
                        }
                        // GPUI can release a control's focus handle before the
                        // next frame reports the loss; keep the host root focused.
                        if !quit && window.focused(cx).is_none() {
                            view.restore_host_focus(window, cx);
                        }
                        if redraw {
                            cx.notify();
                        }
                        if quit {
                            cx.quit();
                        }
                        quit
                    }))
                });
                match result {
                    Ok(Some(false)) => {}
                    Ok(Some(true)) => break,
                    Ok(None) | Err(_) => {
                        let Some(state) = on_window_loss.upgrade() else {
                            break;
                        };
                        {
                            let mut state = state.borrow_mut();
                            if state.quit_requested {
                                break;
                            }
                            state.drain_without_gui("window-unavailable");
                            state.quit_requested = true;
                        }
                        app.update(|cx| cx.quit());
                        break;
                    }
                }
            }
        })
        .detach();
        Self {
            state,
            focus,
            decision_focus: decisions::DecisionFocus::new(cx),
            decision_scroll: gpui::ScrollHandle::new(),
            idle_inhibitor,
            _subscriptions: [bounds, focus_lost],
        }
    }

    /// Restore focus to the persistent host root after a control disappears.
    ///
    /// A focused control owns its [`FocusHandle`]; when it unmounts or drops
    /// `tab_index`, GPUI either keeps `Window::focused` pointing at an id that no
    /// longer renders or blurs the window. Either way the window ends up with no
    /// focused element. The closest still-rendered ancestor (this view's root)
    /// takes focus back, so Tab and the root's native key handling keep working.
    /// This only moves GPUI's in-window focus; it never activates the OS window.
    fn restore_host_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus.contains_focused(window, cx) {
            return;
        }
        let target = window
            .focus_lost_restore_target(cx)
            .unwrap_or_else(|| self.focus.clone());
        target.focus(window, cx);
    }

    fn dismiss_popup(&self, cx: &mut App) -> bool {
        if self.state.borrow().decision_view().identity().is_some() {
            return true;
        }
        let (companion, popup) = {
            let state = self.state.borrow();
            (state.companion, state.chrome.popup())
        };
        if let (Some(companion), Some(popup)) = (companion, popup) {
            let _ = companion.update(cx, |view, window, cx| view.dismiss_popup(popup, window, cx));
            true
        } else {
            false
        }
    }
}

impl Render for HostView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, _| {
                this.state
                    .borrow_mut()
                    .chrome
                    .pointer(event.position, Instant::now());
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    let popup_pending = {
                        let state = this.state.borrow();
                        state.chrome.popup().is_some_and(|popup| {
                            popup.invoker != controls::OPTIONS_ID
                                && state.companion_input_popup != Some(popup)
                        })
                    };
                    if popup_pending {
                        // The companion is still installing the panel shape;
                        // this could be a press inside that panel.
                        cx.stop_propagation();
                        return;
                    }
                    if this.dismiss_popup(cx) {
                        cx.stop_propagation();
                    } else {
                        let mut state = this.state.borrow_mut();
                        state.focus_intent = false;
                        state.keyboard_intent = None;
                        // A video press requests video focus, not a playback
                        // action. The native transfer rechecks family ownership.
                        state.return_focus_intent = state.companion_mode == CompanionMode::Visible;
                    }
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                let key = event.keystroke.key.as_str();
                if key == "tab" && modifiers.number_of_modifiers() == u8::from(modifiers.shift) {
                    let mut state = this.state.borrow_mut();
                    if let Some(identity) = state.decision_view().identity() {
                        if let decisions::Identity::Reset(_) = identity {
                            if !event.is_held {
                                state.keyboard_popup_focus = state.chrome.popup();
                            }
                        } else if !event.is_held {
                            let current = window.focused(cx);
                            let target = if identity == decisions::Identity::DurabilityWarning
                                || current.as_ref() == Some(this.decision_focus.primary())
                            {
                                this.decision_focus.secondary()
                            } else if current.as_ref() == Some(this.decision_focus.secondary()) {
                                this.decision_focus.primary()
                            } else {
                                this.decision_focus.boundary(identity, modifiers.shift)
                            };
                            drop(state);
                            target.focus(window, cx);
                        }
                    } else if state.accepts_input() && !event.is_held {
                        let now = Instant::now();
                        state.chrome.keyboard(false, now);
                        state.keyboard_intent = Some((modifiers.shift, now));
                    }
                    cx.stop_propagation();
                } else if !modifiers.modified() {
                    match key {
                        "f11" => {
                            let mut state = this.state.borrow_mut();
                            if state.decision_view().identity().is_none() {
                                state.request_fullscreen();
                            }
                        }
                        "escape" if !event.is_held => {
                            if !this.dismiss_popup(cx) {
                                let mut state = this.state.borrow_mut();
                                if state.accepts_input() {
                                    let now = Instant::now();
                                    if matches!(state.runtime, RuntimeCoordinator::Capture(_)) {
                                        state.open_capture_menu(settings_menu::Section::Index, now);
                                        state.keyboard_popup_focus = state.chrome.popup();
                                    } else {
                                        state.chrome.open_popup(controls::OPTIONS_ID, now);
                                        state.redraw = true;
                                    }
                                }
                            }
                        }
                        "space" if !event.is_held => {
                            let mut state = this.state.borrow_mut();
                            if state.decision_view().identity().is_none()
                                && state.chrome.popup().is_none()
                                && let Some(action @ Action::TogglePause(..)) = state.action()
                            {
                                state.activate(action);
                            }
                        }
                        _ => return,
                    }
                    cx.stop_propagation();
                }
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, cx| {
                if event.keystroke.key == "f11" {
                    this.state.borrow_mut().f11_release();
                    cx.stop_propagation();
                }
            }))
            .on_scroll(cx.listener(|this, event: &ScrollEvent, _, cx| {
                let outcome = {
                    let mut state = this.state.borrow_mut();
                    // The owned viewport still receives wheel input when the
                    // chrome is unmapped; the event adjusts gain and reveals it.
                    state.volume_wheel(event, false)
                };
                if outcome != volume::WheelOutcome::Ignore {
                    this.state.borrow_mut().chrome.wheel(Instant::now());
                    cx.notify();
                }
            }));
        let state = self.state.borrow();
        let view = &state.view;
        let mut panel = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .p_4()
            .bg(brume::WINDOW)
            .text_color(brume::FOREGROUND);
        if let Some(
            identity @ (decisions::Identity::SaveFailure | decisions::Identity::DurabilityWarning),
        ) = state.decision_view().identity()
        {
            let weak = cx.weak_entity();
            let dispatch: decisions::Dispatch = Rc::new(move |intent, _, _, cx| {
                if let Some(view) = weak.upgrade() {
                    view.update(cx, |this, cx| {
                        if this.state.borrow_mut().dispatch_decision(identity, intent) {
                            cx.notify();
                        }
                    });
                }
            });
            if let Some(body) = decisions::paint(
                state.decision_view(),
                &self.decision_focus,
                &controls::HitRegions::new(),
                Some(&self.decision_scroll),
                &dispatch,
            ) {
                panel = panel.child(
                    body.w(px(decisions::WIDTH))
                        .max_w_full()
                        .p(px(brume::EDGE))
                        .rounded(px(brume::RADIUS_SURFACE))
                        .bg(brume::MENU_SURFACE),
                );
            }
        } else if !state.video_mapped() {
            let snapshot = view.snapshot.as_deref();
            let qualification =
                snapshot.is_some_and(|data| data.session_kind == SessionKind::QualificationVideo);
            let (title, detail) = if state.closing || view.closing {
                ("Closing…", "")
            } else if view.failed {
                ("Video unavailable", "")
            } else if snapshot.is_none() {
                ("Preparing the interface…", "")
            } else if view.disconnected {
                let evidence = snapshot.map_or("", |data| data.recovery.evidence.as_str());
                (
                    "Capture disconnected",
                    if evidence.is_empty() {
                        "Use Options to reconnect a source."
                    } else {
                        evidence
                    },
                )
            } else if view.product_opening {
                if qualification {
                    (
                        "Opening the qualification video…",
                        "Preparing local file playback.",
                    )
                } else {
                    ("Opening capture…", "Validating and opening the source.")
                }
            } else if state
                .lease
                .as_ref()
                .is_some_and(|lease| lease.prepared_paused)
            {
                (
                    "Capture hidden until resume",
                    "Capture will resume when resume is requested.",
                )
            } else if qualification {
                ("Qualification video", "")
            } else if let Some(data) = snapshot
                && (data.selection.is_some() || data.applied.is_some() || data.last_valid.is_some())
            {
                (data.product_phase.as_str(), "")
            } else {
                (
                    "No capture selected",
                    "Choose a source in Options: capture opens immediately.",
                )
            };
            panel = panel
                .child(controls::label(
                    title.to_owned(),
                    controls::LabelVariant::Title,
                ))
                .when(!detail.is_empty(), |panel| panel.child(detail.to_owned()))
                .when(!view.diagnostic.is_empty(), |panel| {
                    panel.child(view.diagnostic.clone())
                });
        }
        input.child(panel)
    }
}

#[link(name = "X11")]
unsafe extern "C" {
    fn XInitThreads() -> c_int;
}

unsafe extern "C" {
    fn setlocale(category: c_int, locale: *const c_char) -> *mut c_char;
}

fn preflight_x11(display: &str) -> Result<X11, &'static str> {
    // Linux <locale.h>, shared by glibc and musl. A null argument queries
    // the process locale without changing it or the environment.
    const LC_NUMERIC: c_int = 1;
    // SAFETY: this is before GPUI startup and the runtime's first native
    // media admission; no other launch thread changes the process C locale.
    let numeric = unsafe { setlocale(LC_NUMERIC, std::ptr::null()) };
    if numeric.is_null() {
        return Err("numeric-locale-query-failed");
    }
    // SAFETY: a successful setlocale query returns a NUL-terminated string
    // valid until the next locale change; nothing mutates it at this point.
    if !matches!(
        unsafe { CStr::from_ptr(numeric) }.to_bytes(),
        b"C" | b"POSIX"
    ) {
        return Err("numeric-locale-not-c");
    }
    // SAFETY: called before GPUI or libmpv can make any Xlib calls.
    if unsafe { XInitThreads() } == 0 {
        return Err("xlib-thread-init-failed");
    }
    X11::connect(display).map_err(|error| match error {
        x11::Error::Connect(_) => "display-unreachable",
        x11::Error::ShapeUnsupported => "shape-unsupported",
        x11::Error::ShapeTooOld { .. } => "shape-version",
        _ => "display-preflight-failed",
    })
}

pub(super) fn run(
    display: &str,
    runtime: RuntimeCoordinator,
    qualification_stdin: bool,
) -> Result<(), GpuiLaunchError> {
    let x11 = match preflight_x11(display) {
        Ok(x11) => Some(x11),
        Err(reason) => {
            tracing::error!(event_code = "gpui_x11_preflight", reason);
            None
        }
    };
    let mut host = HostState::new(runtime, x11, display);
    if host.x11.is_none() {
        host.drain_without_gui("setup-failed");
        tracing::error!(
            event_code = "gpui_exit",
            code = "gpui_native_startup",
            exit_code = 1,
            host = 0_u32,
            authorized = false,
        );
        return Err(GpuiLaunchError::Startup);
    }
    let state = Rc::new(RefCell::new(host));
    let launcher = Rc::clone(&state);
    let mut environment = GraphicalEnvironment::detect();
    environment.x11_display = Some(display.into());
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        gpui_platform::linux(gpui::WindowingModes::X11)
            .with_quit_mode(gpui::QuitMode::Explicit)
            .with_windowing(gpui::WindowingRequest::Windowed(environment))
            .run(move |cx| {
                text_field::register_keys(cx);
                launch(cx, launcher, qualification_stdin);
            });
    }));
    let mut state = state.borrow_mut();
    if outcome.is_err() || !state.quit_requested {
        state.drain_without_gui(if outcome.is_err() {
            "framework-panic"
        } else {
            "framework-return"
        });
    }
    let failure = state.failure.or(state
        .qualification_failure
        .then_some(GpuiLaunchError::QualificationFailure));
    tracing::info!(
        event_code = "gpui_exit",
        generation = state.last_attempt.map_or(0, AttemptId::get),
        root = state.root,
        host = state.last_retired_xid,
        phase = phase_name(state.view.phase),
        exit_code = i32::from(failure.is_some()),
        authorized = state.quit_requested && !state.uncontrolled_exit && outcome.is_ok(),
        controlled = !state.uncontrolled_exit && outcome.is_ok(),
    );
    failure.map_or(Ok(()), Err)
}

fn start_qualification_input(state: &Rc<RefCell<HostState>>) -> bool {
    let (send, receive) = sync_channel(8);
    let worker = std::thread::Builder::new()
        .name("qualification-stdin".into())
        .spawn(move || {
            let mut input = io::stdin().lock();
            let mut line = Vec::new();
            let mut oversized = false;
            loop {
                let chunk = match input.fill_buf() {
                    Ok([]) => {
                        tracing::info!(event_code = "qualification_stdin_eof");
                        return;
                    }
                    Ok(chunk) => chunk,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        tracing::warn!(
                            event_code = "qualification_stdin_read_failed",
                            errno = error.raw_os_error().unwrap_or_default()
                        );
                        return;
                    }
                };
                let newline = chunk.iter().position(|byte| *byte == b'\n');
                let len = newline.unwrap_or(chunk.len());
                if !oversized {
                    if line.len() + len > 65_536 {
                        oversized = true;
                        tracing::warn!(event_code = "qualification_line_oversized");
                    } else {
                        line.extend_from_slice(&chunk[..len]);
                    }
                }
                input.consume(len + usize::from(newline.is_some()));
                if newline.is_none() {
                    continue;
                }
                if !oversized {
                    match std::str::from_utf8(&line) {
                        Ok(line) => match QualificationCommand::parse(line) {
                            Some(command) => {
                                if send.send(command).is_err() {
                                    return;
                                }
                            }
                            None => log_qualification_rejection(),
                        },
                        Err(_) => tracing::warn!(event_code = "qualification_invalid_utf8"),
                    }
                }
                oversized = false;
                line.clear();
            }
        });
    if let Err(error) = worker {
        tracing::warn!(
            event_code = "qualification_stdin_read_failed",
            errno = error.raw_os_error().unwrap_or_default()
        );
        return false;
    }
    state.borrow_mut().qualification_input = Some(receive);
    tracing::info!(event_code = "qualification_stdin_enabled");
    true
}

fn launch(cx: &mut App, state: Rc<RefCell<HostState>>, qualification_stdin: bool) {
    let on_quit = Rc::clone(&state);
    cx.on_app_quit(move |_| {
        // The observer must run synchronously, before GPUI clears the windows.
        let mut state = on_quit.borrow_mut();
        if !state.quit_requested {
            state.drain_without_gui("framework-exit");
        }
        std::future::ready(())
    })
    .detach();
    if start_windows(cx, &state) && (!qualification_stdin || start_qualification_input(&state)) {
        cx.activate(true);
    } else {
        state.borrow_mut().drain_without_gui("setup-failed");
        // The launch callback runs before Calloop; quit on a foreground turn.
        cx.spawn(async move |cx| {
            {
                let mut state = state.borrow_mut();
                if state.quit_requested {
                    return;
                }
                state.quit_requested = true;
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    }
}

fn start_windows(cx: &mut App, state: &Rc<RefCell<HostState>>) -> bool {
    let icon = match image::load_from_memory_with_format(
        include_bytes!("../../assets/logo.png"),
        image::ImageFormat::Png,
    ) {
        Ok(icon) => icon.into_rgba8(),
        Err(error) => {
            tracing::error!(event_code = "gpui_icon_invalid", %error);
            return false;
        }
    };
    let bounds = Bounds::centered(None, size(px(960.), px(600.)), cx);
    let Ok(root) = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(480.), px(540.))),
            titlebar: Some(TitlebarOptions {
                title: Some("Furami".into()),
                ..Default::default()
            }),
            app_id: Some("dev.antho.furami".into()),
            icon: Some(std::sync::Arc::new(icon)),
            window_decorations: Some(WindowDecorations::Server),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| HostView::new(window, cx, Rc::clone(state))),
    ) else {
        return false;
    };
    let Ok(Some((root_xid, scale))) = root.update(cx, |_, window, _| {
        xcb_id(window).map(|xid| (xid, window.scale_factor()))
    }) else {
        return false;
    };
    let hyprland = {
        let mut state = state.borrow_mut();
        let x11 = state.x11.as_mut().expect("preflighted X11 connection");
        if x11
            .bind(root_xid)
            .and_then(|()| x11.set_minimum_scale(scale))
            .is_err()
        {
            return false;
        }
        let hyprland = x11.is_hyprland();
        state.root = x11.parent();
        state.scale = scale;
        hyprland
    };

    let mailbox = Rc::clone(&state.borrow().companion_paint);
    // GPUI maps even with `show: false`. On Hyprland, the backend's +2 X
    // offset gives exactly (0, 0, 1, 1), which XWayland starts hidden.
    // Other WMs keep both staging edges outside the X root.
    let parking = if hyprland {
        Bounds {
            origin: point(px(-2. / scale), px(0.)),
            size: size(px(1. / scale), px(1. / scale)),
        }
    } else {
        let origin = -256. / scale;
        let staging_size = 32. / scale;
        Bounds {
            origin: point(px(origin), px(origin)),
            size: size(px(staging_size), px(staging_size)),
        }
    };
    let Ok(companion) = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(parking)),
            titlebar: None,
            app_id: Some(CHROME_APP_ID.into()),
            focus: false,
            show: false,
            kind: WindowKind::PopUp,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        |window, cx| cx.new(|cx| CompanionView::new(Rc::clone(state), mailbox, window, cx)),
    ) else {
        return false;
    };
    let Ok(Some(companion_xid)) = companion.update(cx, |_, window, _| xcb_id(window)) else {
        return false;
    };
    let mut state = state.borrow_mut();
    let x11 = state.x11.as_mut().expect("preflighted X11 connection");
    if let Err(error) = x11.bind_companion(companion_xid, hyprland) {
        tracing::error!(event_code = "gpui_companion_setup_failed", %error);
        return false;
    }
    let environment = x11.environment_report();
    state.companion = Some(companion);
    state.companion_blur = hyprland.then(CompanionBlur::default);
    if state.resize(true, scale).is_err() {
        return false;
    }
    state.started = true;
    let update = state.runtime.native_environment(environment);
    state.consume(update);
    let update = state.runtime.ui_ready();
    state.consume(update);
    true
}

fn xcb_id(window: &Window) -> Option<u32> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::Xcb(handle) = handle.as_raw() else {
        return None;
    };
    Some(handle.window.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_host_cannot_replay_a_retired_generation() {
        let attempt = AttemptId::new(2).unwrap();
        let next = AttemptId::new(3).unwrap();
        assert!(!can_reserve(
            GatePhase::WaitingSurface,
            AttemptId::new(1).unwrap(),
            Some(attempt)
        ));
        assert!(!can_reserve(
            GatePhase::WaitingSurface,
            attempt,
            Some(attempt)
        ));
        assert!(!can_reserve(GatePhase::Opening, next, Some(attempt)));
        assert!(can_reserve(GatePhase::WaitingSurface, next, Some(attempt)));
    }

    #[test]
    fn gpui_host_release_requires_matching_generation_and_phase() {
        let attempt = AttemptId::new(2).unwrap();
        let lease = Lease {
            attempt,
            xid: None,
            layout: None,
            mapped: false,
            observed_mapped: false,
            prepared_paused: false,
            poisoned: false,
            absent: false,
        };
        assert!(!lease.matches_release(GatePhase::Releasing, AttemptId::new(1).unwrap()));
        assert!(!lease.matches_release(GatePhase::Stopping, attempt));
        assert!(lease.matches_release(GatePhase::Releasing, attempt));
    }

    #[test]
    fn gpui_host_prepared_pause_keeps_attempt_concealed_after_reconnect() {
        let mut lease = Lease {
            attempt: AttemptId::new(1).unwrap(),
            xid: None,
            layout: None,
            mapped: false,
            observed_mapped: false,
            prepared_paused: false,
            poisoned: false,
            absent: false,
        };
        let mut view = ViewState::default();
        assert!(lease.wants_mapped(&view));
        view.disconnected = true;
        assert!(!lease.wants_mapped(&view));
        lease.prepared_paused = true;
        view.disconnected = false;
        assert!(!lease.wants_mapped(&view));
    }

    #[test]
    fn gpui_host_uses_authoritative_action_and_attempt() {
        let attempt = AttemptId::new(2).unwrap();
        let mut view = ViewState {
            attempt: Some(attempt),
            can_toggle_pause: true,
            ..ViewState::default()
        };
        assert_eq!(view.action(), Some(Action::TogglePause(attempt, false)));
        view.paused = true;
        assert_eq!(view.action(), Some(Action::TogglePause(attempt, true)));
        view.closing = true;
        assert_eq!(view.action(), None);
    }

    #[test]
    fn gpui_host_fullscreen_only_confirms_matching_request_before_deadline() {
        let now = Instant::now();
        let request = PendingFullscreen::new(true, true, now);
        assert_eq!(request.observe(false, now), None);
        assert_eq!(request.observe(true, now), Some(true));
        assert_eq!(request.observe(true, now + FULLSCREEN_TIMEOUT), Some(false));
        assert_eq!(
            PendingFullscreen::new(true, false, now).observe(true, now),
            Some(false)
        );
    }

    #[test]
    fn gpui_host_companion_blur_waits_for_and_discards_a_hidden_map_reply() {
        let (tx, rx) = sync_channel(1);
        let mut blur = CompanionBlur {
            blur_map_serial: 1,
            pending: Some(rx),
            ..Default::default()
        };
        assert!(blur.poll(CompanionMode::Unmapped).is_none());
        assert!(blur.pending.is_some());
        assert!(tx.try_send((1, BlurOutcome::Applied)).is_ok());
        assert!(blur.poll(CompanionMode::Unmapped).is_none());
        assert!(blur.pending.is_none());
    }

    #[test]
    fn gpui_host_companion_blur_only_accepts_a_readback_for_the_current_map() {
        let (tx, rx) = sync_channel(1);
        let mut blur = CompanionBlur {
            blur_map_serial: 2,
            pending: Some(rx),
            ..Default::default()
        };
        assert!(tx.try_send((1, BlurOutcome::Applied)).is_ok());
        assert!(blur.poll(CompanionMode::AwaitingBlur(2)).is_none());

        let (tx, rx) = sync_channel(1);
        blur.pending = Some(rx);
        assert!(tx.try_send((2, BlurOutcome::Applied)).is_ok());
        assert!(matches!(
            blur.poll(CompanionMode::AwaitingBlur(2)),
            Some(BlurOutcome::Applied)
        ));

        let (tx, rx) = sync_channel(1);
        blur.pending = Some(rx);
        drop(tx);
        assert!(matches!(
            blur.poll(CompanionMode::AwaitingBlur(2)),
            Some(BlurOutcome::Unavailable)
        ));
    }

    #[test]
    fn gpui_host_companion_blur_bootstrap_is_interrupted_by_a_queued_unmap_then_remap() {
        for (interruption, recovery) in [
            (
                Event::CompanionMappingChanged(false),
                Event::CompanionMappingChanged(true),
            ),
            (
                Event::ParentMappingChanged(false),
                Event::ParentMappingChanged(true),
            ),
            (Event::OwnedFocusDeparted, Event::FocusChanged),
        ] {
            assert!(!recovery.interrupts_blur());
            let mut interrupted = false;
            for event in [interruption, recovery, Event::CompanionChanged] {
                interrupted |= event.interrupts_blur();
            }
            assert!(interrupted);
        }
    }
}
