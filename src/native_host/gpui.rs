//! GUI-thread GPUI adapter. Only the runtime can authorize native retirement.
mod x11;

use std::{
    cell::RefCell,
    ffi::{CStr, c_char, c_int},
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, Context, FocusHandle, GraphicalEnvironment, KeyDownEvent, KeyUpEvent, Render,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowDecorations, WindowOptions, div,
    prelude::*, px, rgb, size,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::{
    app::gate::GatePhase,
    domain::state::AttemptId,
    media::controller::{Generation, SurfaceToken, X11WindowId},
    ui::{
        projection::Snapshot,
        runtime::{RuntimeCoordinator, UiUpdate},
    },
};

use self::x11::{DestroyOutcome, Event, Layout, X11};
// Opaque Brume role colors from qml/Brume.qml.
const MATTE: u32 = 0x24273a;
const SURFACE: u32 = 0x363a4f;
const FOREGROUND: u32 = 0xcad3f5;
const ACCENT: u32 = 0x8aadf4;

const TICK: Duration = Duration::from_millis(16);
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
    phase: GatePhase,
    attempt: Option<AttemptId>,
    snapshot: Option<Rc<Snapshot>>,
    can_open: bool,
    can_toggle_pause: bool,
    paused: bool,
    disconnected: bool,
    failed: bool,
    diagnostic: String,
    settings_status: String,
    close_dialog: String,
    fullscreen: bool,
    closing: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            phase: GatePhase::Idle,
            attempt: None,
            snapshot: None,
            can_open: false,
            can_toggle_pause: false,
            paused: false,
            disconnected: false,
            failed: false,
            diagnostic: String::new(),
            settings_status: String::new(),
            close_dialog: String::new(),
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
    x11: Option<X11>,
    root: u32,
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

impl HostState {
    fn new(runtime: RuntimeCoordinator, x11: Option<X11>) -> Self {
        Self {
            runtime,
            x11,
            root: 0,
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
        // Dropping the X11 connection would itself destroy its windows. Like
        // Qt's fatal barrier, do not unwind or claim a successful release.
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
            product_phase,
            failed,
            can_open,
            can_toggle_pause,
            paused,
            context_diagnostic,
            settings_status,
            close_dialog,
            fullscreen,
            closing,
            prepared_paused,
            terminal_failure,
            create_native,
            release_native,
            quit,
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
            self.view.phase = phase;
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
            self.view.failed = failed;
            self.view.can_open = can_open;
            self.view.can_toggle_pause = can_toggle_pause;
            self.view.paused = paused;
            self.view.diagnostic = context_diagnostic;
            self.view.settings_status = settings_status;
            self.view.close_dialog = close_dialog;
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
        self.control_lost |= error.is_connection_loss();
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
    }

    fn accepts_input(&self) -> bool {
        !self.closing
            && !self.view.closing
            && self.failure.is_none()
            && !self.qualification_failure
            && !self.quit_authorized
    }

    fn video_mapped(&self) -> bool {
        self.lease.as_ref().is_some_and(|lease| lease.mapped)
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

    /// Drain native events first, and return whether to issue the saved-mode
    /// toggle outside this borrow.
    fn tick(&mut self, scale: f32, fullscreen: bool) -> bool {
        let mut geometry_changed = std::mem::take(&mut self.bounds_changed);
        if !self.root_lost && !self.control_lost {
            loop {
                match self.x11().next_event(self.xid()) {
                    Ok(Some(Event::GeometryChanged)) => geometry_changed = true,
                    Ok(Some(Event::ParentDestroyed)) => {
                        self.report_loss("root-destroyed", false, true);
                        break;
                    }
                    Ok(Some(Event::ChildDestroyed)) => {
                        self.report_loss("child-destroyed", true, false)
                    }
                    Ok(Some(Event::ChildReparented)) => {
                        self.report_loss("child-reparented", false, false)
                    }
                    Ok(None) => break,
                    Err(error) => {
                        self.report_x11_error("x11-event-failed", &error);
                        break;
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
        if self.root_lost || self.control_lost || self.quit_authorized {
            return false;
        }
        if let Err(error) = self.update_mapping() {
            self.report_x11_error("mapping-failed", &error);
            return false;
        }
        if !self.accepts_input() {
            self.cancel_fullscreen();
            return false;
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
                    return false;
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
                return true;
            }
        }
        false
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

struct HostView {
    state: Rc<RefCell<HostState>>,
    focus: FocusHandle,
    _subscriptions: [Subscription; 3],
}

impl HostView {
    fn new(window: &mut Window, cx: &mut Context<Self>, state: Rc<RefCell<HostState>>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let bounds = cx.observe_window_bounds(window, |this, _, _| {
            this.state.borrow_mut().bounds_changed = true;
        });
        let activation = cx.observe_window_activation(window, |this, window, _| {
            if !window.is_window_active() {
                this.state.borrow_mut().f11_down = false;
            }
        });
        // A control that unmounts (or stops being a tab stop) takes its focus
        // handle with it, which leaves the window with no focused element.
        let focus_lost = cx.on_focus_lost(window, |this, window, cx| {
            this.restore_host_focus(window, cx);
        });
        let on_close = Rc::downgrade(&state);
        window.on_window_should_close(cx, move |window, cx| {
            let on_close = on_close.clone();
            window.defer(cx, move |_, _| {
                if let Some(state) = on_close.upgrade() {
                    state.borrow_mut().wm_close();
                }
            });
            false
        });
        let on_window_loss = Rc::downgrade(&state);
        let app = cx.to_async();
        let background = cx.background_executor().clone();
        cx.spawn_in(window, async move |view, cx| {
            loop {
                background.timer(TICK).await;
                let result = cx.update(|window, cx| {
                    let view = view.upgrade()?;
                    Some(view.update(cx, |view, cx| {
                        let (redraw, quit, restore_fullscreen) = {
                            let mut state = view.state.borrow_mut();
                            // Keep reading native events while the owner stops.
                            let restore = state.tick(window.scale_factor(), window.is_fullscreen());
                            let quit = state.quit_authorized && !state.quit_requested;
                            state.quit_requested |= quit;
                            (std::mem::take(&mut state.redraw), quit, restore)
                        };
                        if restore_fullscreen {
                            window.toggle_fullscreen();
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
            _subscriptions: [bounds, activation, focus_lost],
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
}

impl Render for HostView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if event.keystroke.key == "f11" && !modifiers.modified() {
                    let request = this.state.borrow_mut().f11_press(window.is_fullscreen());
                    if request {
                        window.toggle_fullscreen();
                    }
                    cx.stop_propagation();
                } else if event.keystroke.key == "tab"
                    && modifiers.number_of_modifiers() == u8::from(modifiers.shift)
                {
                    if modifiers.shift {
                        window.focus_prev(cx);
                    } else {
                        window.focus_next(cx);
                    }
                    cx.stop_propagation();
                }
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, cx| {
                if event.keystroke.key == "f11" {
                    this.state.borrow_mut().f11_release();
                    cx.stop_propagation();
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
            .bg(rgb(MATTE))
            .text_color(rgb(FOREGROUND));
        let overlay = if !view.close_dialog.is_empty() {
            let save_failed = view.close_dialog == "save";
            panel = panel.child(view.settings_status.clone());
            if save_failed {
                panel = panel.child(decision_button("retry-save", "Retry save").on_click(
                    cx.listener(|this, _, _, cx| {
                        let mut state = this.state.borrow_mut();
                        if state.view.close_dialog == "save" {
                            let update = state.runtime.retry_save();
                            state.consume(update);
                            cx.notify();
                        }
                    }),
                ));
            }
            panel = panel.child(
                decision_button(
                    "close-without-save",
                    if save_failed {
                        "Close without saving"
                    } else {
                        "Close anyway"
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    let mut state = this.state.borrow_mut();
                    if matches!(state.view.close_dialog.as_str(), "save" | "warning") {
                        let update = state.runtime.close_without_save();
                        state.consume(update);
                        cx.notify();
                    }
                })),
            );
            None
        } else {
            let message = if state.closing || view.closing {
                "Closing…"
            } else if view.failed || view.disconnected {
                "Video unavailable"
            } else if view
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.opening)
            {
                "Opening video…"
            } else if view.phase == GatePhase::Ready {
                "Video ready"
            } else {
                "No video open"
            };
            let mapped = state.video_mapped();
            if !mapped {
                panel = panel
                    .child(message)
                    .when(!view.diagnostic.is_empty(), |panel| {
                        panel.child(view.diagnostic.clone())
                    });
            }
            let action = state.action();
            let label = match action {
                Some(Action::Open) => "Open capture",
                Some(Action::TogglePause(_, true)) => "Resume",
                Some(Action::TogglePause(_, false)) => "Pause",
                None => message,
            };
            state
                .layout
                .filter(|_| mapped || action.is_some())
                .map(|layout| overlay_button(layout, state.scale, label, action, cx))
        };
        input.child(panel).children(overlay)
    }
}

fn overlay_button(
    layout: Layout,
    scale: f32,
    label: &'static str,
    action: Option<Action>,
    cx: &mut Context<HostView>,
) -> gpui::Stateful<gpui::Div> {
    let rect = layout.button;
    let logical = |pixels| px(f32::from(pixels) / scale);
    div()
        .id("capture-control")
        .absolute()
        .left(logical(rect.left))
        .top(logical(rect.top))
        .w(logical(rect.right - rect.left))
        .h(logical(rect.bottom - rect.top))
        .overflow_hidden()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(SURFACE))
        .text_color(rgb(FOREGROUND))
        // Keep the same focus node through a pending pause/resume. It is not
        // a tab target and has no action until the runtime confirms the edge.
        .tab_index(0)
        .tab_stop(action.is_some())
        .child(label)
        .when_some(action, |button, action| {
            // GPUI also invokes this on focused Enter/Space release.
            button
                .cursor_pointer()
                .focus(|style| style.text_color(rgb(ACCENT)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.state.borrow_mut().activate(action);
                    cx.notify();
                }))
        })
}

fn decision_button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .tab_index(0)
        .cursor_pointer()
        .bg(rgb(SURFACE))
        .focus(|style| style.text_color(rgb(ACCENT)))
        .py_2()
        .px_4()
        .child(label)
}

unsafe extern "C" {
    fn XInitThreads() -> c_int;
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

pub(super) fn run(display: &str, runtime: RuntimeCoordinator) -> Result<(), GpuiLaunchError> {
    let x11 = match preflight_x11(display) {
        Ok(x11) => Some(x11),
        Err(reason) => {
            tracing::error!(event_code = "gpui_x11_preflight", reason);
            None
        }
    };
    let mut host = HostState::new(runtime, x11);
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
            .run(move |cx| launch(cx, launcher));
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

fn launch(cx: &mut App, state: Rc<RefCell<HostState>>) {
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
    let bounds = Bounds::centered(None, size(px(960.), px(540.)), cx);
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(480.), px(540.))),
            titlebar: Some(TitlebarOptions {
                title: Some("Furami · GPUI".into()),
                ..Default::default()
            }),
            app_id: Some("dev.antho.furami".into()),
            window_decorations: Some(WindowDecorations::Server),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| HostView::new(window, cx, Rc::clone(&state))),
    );
    if let Ok(window) = window
        && matches!(
            window.update(cx, |_, window, _| bind_and_start(window, &state)),
            Ok(true)
        )
    {
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

fn bind_and_start(window: &Window, state: &RefCell<HostState>) -> bool {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return false;
    };
    let RawWindowHandle::Xcb(handle) = handle.as_raw() else {
        return false;
    };
    let mut state = state.borrow_mut();
    let x11 = state.x11.as_mut().expect("preflighted X11 connection");
    let scale = window.scale_factor();
    if x11.bind(handle.window.get()).is_err() || x11.set_minimum_scale(scale).is_err() {
        return false;
    }
    state.root = x11.parent();
    state.scale = scale;
    if state.resize(true, scale).is_err() {
        return false;
    }
    state.started = true;
    let update = state.runtime.ui_ready();
    state.consume(update);
    true
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
}
