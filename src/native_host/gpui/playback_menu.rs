//! Brume Capture root menu for the GPUI frontend.
//!
//! The menu is title, compiled version, live playback summary, then
//! Pause/Resume, Restart, Reconnect, Open, Settings, Open logs folder and
//! Close. The settings index and its sections are not implemented here;
//! Settings activation is handed to the parent through one callback so this
//! file never fabricates a page.
//!
//! Identity rules:
//!
//! - Each row is built with the *rendered* projection: the pausable attempt
//!   comes from [`HostState::action`], Restart and Reconnect bind
//!   `ViewState::restart_generation` (`UiUpdate::restart_generation`, the token
//!   the runtime gate consumes for restart/reconnect), and Close binds
//!   `ViewState::generation` (`UiUpdate::generation`, the token close dispatch
//!   uses). The snapshot's `generation` string is a UI guard only and is never
//!   used as a dispatch token.
//! - Every dispatch re-checks the rendered popup identity and the current
//!   runtime guards before handing the intent to `RuntimeCoordinator`; the
//!   returned `UiUpdate` goes through `HostState::consume`.
//!
//! Host projection consumed here: `ViewState::{generation,
//! restart_generation, can_restart, logging_enabled}`, all carried from the
//! same `UiUpdate`. `ViewState::attempt` is filtered to native-lifecycle
//! phases and must not stand in for the Close generation.
//!
//! Host integration surface: the parent owns the native panel, its placement,
//! focus transfer and hit regions. [`PlaybackMenu::new`] creates the fixed
//! per-row focus handles, [`PlaybackMenu::focus_targets`] feeds the popup Tab
//! chain, and [`paint`] renders the panel content.

use std::{cell::RefCell, rc::Rc};

use gpui::{
    Context, Div, FocusHandle, Role, SharedString, Stateful, WeakEntity, div, prelude::*, px,
};

use crate::{
    app::gate::GatePhase,
    domain::state::AttemptId,
    ui::projection::{Live, Snapshot},
};

use super::{
    Action, HostState, brume,
    chrome::Popup,
    companion_view::CompanionView,
    controls::{self, Activate, Activation, HitRegions, rate_fps},
    top_chrome::FocusTarget,
};

/// Stable element identities, so the accessible names stay put across paints.
pub(super) const ROOT_ID: &str = "menu-root";
pub(super) const SUBTITLE_ID: &str = "menuSubtitle";
pub(super) const PAUSE_ID: &str = "menuPauseResume";
pub(super) const RESTART_ID: &str = "menuRestart";
pub(super) const RECONNECT_ID: &str = "menuReconnect";
pub(super) const OPEN_ID: &str = "menuOpenCapture";
pub(super) const SETTINGS_ID: &str = "menuSettings";
pub(super) const LOGS_ID: &str = "menuOpenLogs";
pub(super) const CLOSE_ID: &str = "menuCloseCapture";

/// Per-paint values the host supplies for this panel.
pub(super) struct MenuInputs<'a> {
    /// The popup epoch/identity this panel was rendered for.
    pub(super) popup: Popup,
    /// Opens the settings index; the parent owns that navigation.
    pub(super) on_settings: &'a Activate,
}

/// Facts the pure projection consumes, so the gates can be tested without a
/// live `HostState`.
struct MenuFacts<'a> {
    qualification: bool,
    can_open: bool,
    can_restart: bool,
    restart_generation: u64,
    /// `ViewState::generation`, the Close row's dispatch token.
    native_generation: u64,
    pause: Option<(AttemptId, bool)>,
    phase: GatePhase,
    paused: bool,
    closing: bool,
    snapshot: Option<&'a Snapshot>,
    logging_enabled: bool,
}

/// The Capture root menu's rendered model. Visibility and enablement come from
/// the runtime gates; `*_generation` fields are the exact tokens the row binds.
struct MenuModel {
    title: &'static str,
    version: &'static str,
    subtitle: String,
    paused: bool,
    pause: Option<(AttemptId, bool)>,
    show_restart: bool,
    restart_generation: u64,
    restart_enabled: bool,
    reconnect_visible: bool,
    reconnect_generation: u64,
    reconnect_enabled: bool,
    open_visible: bool,
    settings_visible: bool,
    logs_visible: bool,
    logs_enabled: bool,
    close_generation: u64,
    close_enabled: bool,
    close_label: &'static str,
}

impl MenuModel {
    fn project(facts: &MenuFacts<'_>) -> Self {
        let snapshot = facts.snapshot;
        let closing = snapshot.map_or(facts.closing, |snapshot| snapshot.closing);
        let reconnect_visible = snapshot.is_some_and(|snapshot| snapshot.can_reconnect);
        let logs_visible = !facts.qualification && snapshot.is_some() && facts.logging_enabled;
        Self {
            title: if facts.qualification {
                "Qualification video"
            } else {
                "Capture"
            },
            version: env!("CARGO_PKG_VERSION"),
            subtitle: subtitle(
                &snapshot.map_or_else(
                    || super::phase_name(facts.phase).to_owned(),
                    |snapshot| snapshot.product_phase.clone(),
                ),
                snapshot.map_or(&EMPTY_LIVE, |snapshot| &snapshot.live),
            ),
            paused: snapshot.map_or(facts.paused, |snapshot| snapshot.paused),
            pause: facts.pause,
            // Restart is hidden for the qualification video.
            show_restart: !facts.qualification,
            // The dispatch token is `UiUpdate.restart_generation`, the same
            // token the runtime's restart gate consumes.
            restart_generation: facts.restart_generation,
            restart_enabled: !facts.qualification && facts.can_restart,
            reconnect_visible,
            // Reconnect shares the restart token (`reconnectCapture`).
            reconnect_generation: facts.restart_generation,
            // Enabled while the session can reconnect and no close is in
            // flight (`canReconnect && !closing`).
            reconnect_enabled: reconnect_visible && !closing,
            // `openable && !canReconnect`; `openable` is the projected
            // `UiUpdate.can_open`, carried by `ViewState`, not the snapshot.
            open_visible: facts.can_open && !reconnect_visible,
            settings_visible: !facts.qualification,
            logs_visible,
            // The entry exists only while logging is opted in; once shown it
            // stays available without capture and with an unavailable sink,
            // and only an in-flight preparation or the close disables it.
            logs_enabled: logs_visible
                && !closing
                && snapshot
                    .is_some_and(|snapshot| snapshot.diagnostics.folder_state != "Preparing"),
            // `closeCapture()` dispatches with the projected native
            // generation, not the state-identity restart token.
            close_generation: facts.native_generation,
            close_enabled: !closing,
            close_label: if facts.qualification {
                "Close qualification"
            } else {
                "Close capture"
            },
        }
    }

    fn from_host(state: &HostState) -> Self {
        let pause = match state.action() {
            Some(Action::TogglePause(attempt, paused)) => Some((attempt, paused)),
            _ => None,
        };
        let facts = MenuFacts {
            qualification: matches!(state.runtime, super::RuntimeCoordinator::Qualification(_)),
            can_open: state.view.can_open,
            can_restart: state.view.can_restart,
            restart_generation: state.view.restart_generation,
            native_generation: state.view.generation,
            pause,
            phase: state.view.phase,
            paused: state.view.paused,
            closing: state.view.closing,
            snapshot: state.view.snapshot.as_deref(),
            logging_enabled: state.view.logging_enabled,
        };
        Self::project(&facts)
    }

    fn pause_enabled(&self) -> bool {
        self.pause.is_some()
    }
}

/// The `Live` fallback used when no snapshot is published yet; its empty
/// label fields keep the subtitle truthful instead of inventing a selection.
static EMPTY_LIVE: Live = Live {
    source_label: String::new(),
    size_label: String::new(),
    rate_label: String::new(),
    format_label: String::new(),
    audio_status: String::new(),
    audio_diagnostic: String::new(),
    playback_status: String::new(),
    volume_percent: 0,
    muted: false,
};

/// Display-only product phase vocabulary. Unknown tokens are shown verbatim;
/// this never decides action legality.
fn product_phase_text(phase: &str) -> &str {
    match phase {
        "Idle" => "No capture",
        "Stopped" => "Capture stopped",
        "Active" => "Playing",
        "Paused" => "Playback paused",
        "PausePending" => "Pausing…",
        "Validating" => "Validating…",
        "ClosingOld" => "Closing media…",
        "ApplyingTreatments" => "Applying treatments…",
        "RestoringTreatments" => "Restoring treatments…",
        "OpeningCandidate" => "Opening source…",
        "CleaningFailedCandidate" => "Cleaning up after failure…",
        "ValidatingPrior" => "Validating previous session…",
        "OpeningRestore" => "Restoring media…",
        "CleaningFailedRestore" => "Cleaning up after failure…",
        "ValidatingResume" => "Validating resume…",
        "ClosingResume" => "Closing for resume…",
        "OpeningResume" => "Resuming playback…",
        "CleaningFailedResume" => "Cleaning up after failure…",
        "ErrorWithActiveRestored" => "Error — capture restored",
        "ErrorWithoutActive" => "Error — no active capture",
        "Disconnected" => "Capture disconnected",
        "Recovering" => "Recovering…",
        "SelectionRequired" => "Source required",
        "Stopping" => "Stopping…",
        "ShutdownReady" => "Shutdown complete",
        other => other,
    }
}

/// The menu subtitle: the product phase plus the verified live source, size
/// and rate labels, each joined with " · ".
fn subtitle(product_phase: &str, live: &Live) -> String {
    let mut text = product_phase_text(product_phase).to_owned();
    for label in [&live.source_label, &live.size_label] {
        if !label.is_empty() {
            text.push_str(" · ");
            text.push_str(label);
        }
    }
    if !live.rate_label.is_empty() {
        text.push_str(" · ");
        text.push_str(&rate_fps(&live.rate_label));
    }
    text
}

/// One rendered row dispatch. Every variant carries the value rendered when
/// the row was built, never a freshly fetched one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Intent {
    TogglePause { attempt: AttemptId, paused: bool },
    Restart { generation: u64 },
    Reconnect { generation: u64 },
    Open,
    OpenLogs,
    Close { generation: u64 },
}

impl HostState {
    /// Dispatch one menu intent through the checked runtime intents.
    ///
    /// The surface must still be the rendered popup and the host must still
    /// accept input; stale generations then reach the runtime's own guard and
    /// their refusal is projected through `consume`.
    fn playback_menu_dispatch(&mut self, popup: Popup, intent: Intent) -> bool {
        if self.chrome.popup() != Some(popup)
            || !self.accepts_input()
            || !self.view.close_dialog.is_empty()
        {
            return false;
        }
        match intent {
            // The same guarded path as the gate ribbon: re-verifies the
            // rendered action before toggling and consumes the update.
            Intent::TogglePause { attempt, paused } => {
                self.activate(Action::TogglePause(attempt, paused))
            }
            Intent::Restart { generation } => {
                let update = self.runtime.restart(generation);
                self.consume(update);
            }
            Intent::Reconnect { generation } => {
                let update = self.runtime.reconnect(generation);
                self.consume(update);
            }
            Intent::Open => {
                let update = self.runtime.open();
                self.consume(update);
            }
            Intent::OpenLogs => {
                let update = self.runtime.open_log_folder();
                self.consume(update);
            }
            Intent::Close { generation } => {
                let update = self.runtime.close(generation);
                self.consume(update);
            }
        }
        true
    }
}

/// Fixed per-row focus handles owned by the parent view, so Pause↔Resume and
/// every conditional row keep one mutable GPUI focus node.
pub(super) struct PlaybackMenu {
    pause: FocusHandle,
    restart: FocusHandle,
    reconnect: FocusHandle,
    open: FocusHandle,
    settings: FocusHandle,
    logs: FocusHandle,
    close: FocusHandle,
}

impl PlaybackMenu {
    pub(super) fn new(cx: &mut Context<CompanionView>) -> Self {
        Self {
            pause: cx.focus_handle(),
            restart: cx.focus_handle(),
            reconnect: cx.focus_handle(),
            open: cx.focus_handle(),
            settings: cx.focus_handle(),
            logs: cx.focus_handle(),
            close: cx.focus_handle(),
        }
    }

    /// The popup's Tab chain in paint order, skipping hidden and disabled
    /// rows.
    pub(super) fn focus_targets(&self, state: &HostState) -> Vec<FocusTarget> {
        let model = MenuModel::from_host(state);
        let mut targets = Vec::new();
        if model.pause_enabled() {
            targets.push(FocusTarget::plain(&self.pause));
        }
        if model.restart_enabled {
            targets.push(FocusTarget::plain(&self.restart));
        }
        if model.reconnect_enabled {
            targets.push(FocusTarget::plain(&self.reconnect));
        }
        if model.open_visible {
            targets.push(FocusTarget::plain(&self.open));
        }
        if model.settings_visible {
            targets.push(FocusTarget::plain(&self.settings));
        }
        if model.logs_enabled {
            targets.push(FocusTarget::plain(&self.logs));
        }
        if model.close_enabled {
            targets.push(FocusTarget::plain(&self.close));
        }
        targets
    }
}

/// Render the Capture root menu content. The caller places it in the owned
/// panel; this function never maps, positions or focuses a window.
pub(super) fn paint(
    menu: &PlaybackMenu,
    state: &Rc<RefCell<HostState>>,
    inputs: &MenuInputs<'_>,
    regions: &HitRegions,
    cx: &mut Context<CompanionView>,
) -> Stateful<Div> {
    let model = {
        let state = state.borrow();
        MenuModel::from_host(&state)
    };
    let weak = cx.weak_entity();
    let popup = inputs.popup;
    let pause_enabled = model.pause_enabled();

    let pause_label = if model.paused { "Resume" } else { "Pause" };
    let pause_icon = brume::icon(if model.paused { "play" } else { "pause" });
    let pause_activate = model.pause.map(|(attempt, paused)| {
        menu_activation(
            state,
            weak.clone(),
            popup,
            Intent::TogglePause { attempt, paused },
        )
    });

    let restart_activate = model.restart_enabled.then(|| {
        menu_activation(
            state,
            weak.clone(),
            popup,
            Intent::Restart {
                generation: model.restart_generation,
            },
        )
    });

    let reconnect_activate = model.reconnect_enabled.then(|| {
        menu_activation(
            state,
            weak.clone(),
            popup,
            Intent::Reconnect {
                generation: model.reconnect_generation,
            },
        )
    });

    let open_activate = model
        .open_visible
        .then(|| menu_activation(state, weak.clone(), popup, Intent::Open));

    let logs_activate = model
        .logs_enabled
        .then(|| menu_activation(state, weak.clone(), popup, Intent::OpenLogs));

    let close_activate = model.close_enabled.then(|| {
        menu_activation(
            state,
            weak.clone(),
            popup,
            Intent::Close {
                generation: model.close_generation,
            },
        )
    });

    let settings_activate = model.settings_visible.then(|| {
        // Settings navigation belongs to the parent. Guard the popup identity
        // so an obsolete panel cannot open a replacement page.
        let state = Rc::clone(state);
        let on_settings = Rc::clone(inputs.on_settings);
        let activate: Activate = Rc::new(move |source, window, cx| {
            let rendered = {
                let state = state.borrow();
                state.chrome.popup().is_some_and(|current| current == popup)
                    && state.accepts_input()
            };
            if rendered {
                on_settings(source, window, cx);
            }
        });
        activate
    });

    let head = div()
        .flex()
        .flex_col()
        .child(super::settings_menu::heading(
            model.title,
            model.version,
            None,
        ))
        .child(
            note(model.subtitle)
                .id(SUBTITLE_ID)
                .w_full()
                .overflow_hidden(),
        );

    let rows = div()
        .flex()
        .flex_col()
        .gap(px(brume::GAP_SMALL))
        .child(
            controls::button(
                PAUSE_ID,
                pause_label,
                pause_icon,
                pause_enabled,
                regions,
                pause_activate,
            )
            .when(pause_enabled, |button| button.track_focus(&menu.pause))
            .w_full(),
        )
        .when(model.show_restart, |rows| {
            rows.child(
                controls::button(
                    RESTART_ID,
                    "Restart capture",
                    brume::icon("refresh"),
                    model.restart_enabled,
                    regions,
                    restart_activate,
                )
                .when(model.restart_enabled, |button| {
                    button.track_focus(&menu.restart)
                })
                .w_full(),
            )
        })
        .when(model.reconnect_visible, |rows| {
            rows.child(
                controls::button(
                    RECONNECT_ID,
                    "Reconnect capture",
                    brume::icon("refresh"),
                    model.reconnect_enabled,
                    regions,
                    reconnect_activate,
                )
                .when(model.reconnect_enabled, |button| {
                    button.track_focus(&menu.reconnect)
                })
                .w_full(),
            )
        })
        .when(model.open_visible, |rows| {
            rows.child(
                controls::button(
                    OPEN_ID,
                    "Open capture",
                    brume::icon("play"),
                    true,
                    regions,
                    open_activate,
                )
                .track_focus(&menu.open)
                .w_full(),
            )
        })
        .when(model.settings_visible, |rows| {
            rows.child(
                controls::button(
                    SETTINGS_ID,
                    "Settings",
                    brume::icon("settings"),
                    true,
                    regions,
                    settings_activate,
                )
                .track_focus(&menu.settings)
                .w_full(),
            )
        })
        .when(model.logs_visible, |rows| {
            rows.child(
                controls::button(
                    LOGS_ID,
                    "Open logs folder",
                    None,
                    model.logs_enabled,
                    regions,
                    logs_activate,
                )
                .when(model.logs_enabled, |button| button.track_focus(&menu.logs))
                .w_full(),
            )
        })
        .child(
            controls::button(
                CLOSE_ID,
                model.close_label,
                brume::icon("x"),
                model.close_enabled,
                regions,
                close_activate,
            )
            .when(model.close_enabled, |button| {
                button.track_focus(&menu.close)
            })
            .w_full(),
        );

    div()
        .id(ROOT_ID)
        .accessibility_id(ROOT_ID)
        .role(Role::Group)
        .aria_label(model.title)
        .flex()
        .flex_col()
        .gap(px(brume::GAP))
        .w_full()
        .child(head)
        .child(rows)
}

/// Brume `note` label. The shared control module only exposes the title
/// variant today; `controls::label` should absorb this variant when its
/// consumer slice lands.
fn note(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(brume::FONT_SIZE_SMALL))
        .font_weight(brume::FONT_WEIGHT_NORMAL)
        .text_color(brume::SECONDARY)
        .child(text.into())
}

/// Activation for one rendered row: the popup identity, host guards and
/// runtime generation were all captured at paint time.
fn menu_activation(
    state: &Rc<RefCell<HostState>>,
    view: WeakEntity<CompanionView>,
    popup: Popup,
    intent: Intent,
) -> Activate {
    let state = Rc::clone(state);
    Rc::new(move |source, _window, cx| {
        {
            let mut state = state.borrow_mut();
            if !state.playback_menu_dispatch(popup, intent) {
                return;
            }
            state.focus_intent |= source == Activation::Accessibility;
        }
        let _ = view.update(cx, |_, cx| cx.notify());
    })
}

#[cfg(test)]
mod tests {
    use super::{MenuFacts, MenuModel, product_phase_text, subtitle};
    use crate::{
        app::gate::GatePhase,
        domain::state::AttemptId,
        ui::projection::{
            Catalog, Diagnostics, Live, Output, Recovery, Renderer, SessionKind, Settings, Snapshot,
        },
    };

    fn snapshot(can_reconnect: bool, closing: bool) -> Snapshot {
        Snapshot {
            schema: 2,
            session_kind: SessionKind::Capture,
            state_token: "state:1".into(),
            // The UI-guard token; never a dispatch generation.
            generation: "7".into(),
            product_phase: "Active".into(),
            gate_phase: "Ready".into(),
            paused: false,
            opening: false,
            closing,
            can_edit_selection: true,
            can_restart: false,
            can_reconnect,
            can_toggle_pause: true,
            can_set_gain: true,
            selection_revision: "1".into(),
            selection: None,
            applied: None,
            last_valid: None,
            live: Live::default(),
            stream_stats: None,
            catalog: Catalog::default(),
            renderer: Renderer::default(),
            output: Output {
                revision: "1".into(),
                ..Default::default()
            },
            recovery: Recovery::default(),
            settings: Settings::default(),
            diagnostics: Diagnostics {
                status: "Healthy",
                path: String::new(),
                message: "",
                has_problem: false,
                dropped_events: "0".into(),
                oversized_records: "0".into(),
                oversized_reports: "0".into(),
                folder_state: "Idle",
            },
            intent_error: None,
        }
    }

    fn fixture<'a>(snapshot: Option<&'a Snapshot>) -> MenuFacts<'a> {
        MenuFacts {
            qualification: false,
            can_open: false,
            can_restart: true,
            restart_generation: 5,
            native_generation: 9,
            pause: Some((AttemptId::new(3).expect("attempt"), false)),
            phase: GatePhase::Ready,
            paused: false,
            closing: false,
            snapshot,
            logging_enabled: false,
        }
    }

    #[test]
    fn restart_reconnect_and_close_bind_their_own_generations() {
        let snapshot = snapshot(true, false);
        let mut facts = fixture(Some(&snapshot));
        facts.restart_generation = 7;
        facts.native_generation = 42;
        let model = MenuModel::project(&facts);
        // Restart/Reconnect use the state-identity restart token; Close uses
        // the projected native generation.
        assert_eq!(model.restart_generation, 7);
        assert_eq!(model.reconnect_generation, 7);
        assert_eq!(model.close_generation, 42);
        assert!(model.restart_enabled);
        assert!(model.reconnect_enabled);
        assert!(model.close_enabled);
        // A reconnectable session hides Open capture even when can_open is set.
        let mut open = facts;
        open.can_open = true;
        assert!(!MenuModel::project(&open).open_visible);
    }

    #[test]
    fn restart_gate_comes_from_the_view_state_can_restart() {
        let snapshot = snapshot(false, false);
        let mut facts = fixture(Some(&snapshot));
        facts.can_restart = false;
        let model = MenuModel::project(&facts);
        assert!(model.show_restart, "the row stays visible but disabled");
        assert!(!model.restart_enabled);

        let mut facts = fixture(Some(&snapshot));
        facts.can_restart = true;
        assert!(MenuModel::project(&facts).restart_enabled);
    }

    #[test]
    fn closing_disables_close_and_reconnect_but_keeps_restart_projected() {
        let snapshot = snapshot(true, true);
        let model = MenuModel::project(&fixture(Some(&snapshot)));
        assert!(!model.close_enabled);
        assert!(!model.reconnect_enabled);
        assert!(model.restart_enabled, "can_restart still drives Restart");
        assert_eq!(
            model.close_generation, 9,
            "the rendered token survives the disabled row"
        );
    }

    #[test]
    fn open_requires_can_open_without_reconnect() {
        let snapshot = snapshot(false, false);
        let mut open = fixture(Some(&snapshot));
        open.can_open = true;
        let model = MenuModel::project(&open);
        assert!(model.open_visible);
        assert!(!model.reconnect_visible);
    }

    #[test]
    fn qualification_hides_restart_settings_and_logs() {
        let snapshot = snapshot(false, false);
        let mut facts = fixture(Some(&snapshot));
        facts.qualification = true;
        facts.can_restart = false;
        facts.logging_enabled = true;
        let model = MenuModel::project(&facts);
        assert_eq!(model.title, "Qualification video");
        assert!(!model.show_restart);
        assert!(!model.settings_visible);
        assert!(!model.logs_visible);
        assert_eq!(model.close_label, "Close qualification");
    }

    #[test]
    fn logs_follow_the_opt_in_and_folder_preparation() {
        let mut snapshot = snapshot(false, false);
        snapshot.diagnostics.folder_state = "Preparing";
        let mut facts = fixture(Some(&snapshot));
        facts.logging_enabled = true;
        let model = MenuModel::project(&facts);
        assert!(model.logs_visible);
        assert!(!model.logs_enabled, "preparing folder disables the row");

        let mut facts = fixture(Some(&snapshot));
        facts.logging_enabled = false;
        assert!(!MenuModel::project(&facts).logs_visible);
    }

    #[test]
    fn absent_snapshot_renders_the_real_gate_phase() {
        let mut facts = fixture(None);
        facts.phase = GatePhase::WaitingSurface;
        facts.closing = true;
        let model = MenuModel::project(&facts);
        assert_eq!(model.subtitle, "WaitingSurface");
        assert!(!model.close_enabled, "closing falls back to ViewState");
    }

    #[test]
    fn subtitle_matches_the_join_order() {
        let live = Live {
            source_label: "ShadowCast 3".into(),
            size_label: "1920×1080".into(),
            rate_label: "60000/1001".into(),
            ..Default::default()
        };
        assert_eq!(
            subtitle("Active", &live),
            "Playing · ShadowCast 3 · 1920×1080 · 59.94 FPS"
        );
        assert_eq!(subtitle("Active", &Live::default()), "Playing");
        assert_eq!(
            subtitle("", &Live::default()),
            product_phase_text(""),
            "unknown phases stay verbatim"
        );
    }
}
