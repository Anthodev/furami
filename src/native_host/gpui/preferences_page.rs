//! Brume Preferences settings body for the GPUI frontend.
//!
//! The page holds the immediate playback preferences (show stream statistics,
//! diagnostic logging), the saved-file reset intent, and a local
//! never-persisted "Technical details" disclosure. Persistence attention stays
//! outside the disclosure: a refusal, pending legacy migration, write
//! durability problem or startup restoration fault must be visible whatever
//! the disclosure state is.
//!
//! Identity rules:
//!
//! - "Show stream statistics" is the positive control: it is checked while
//!   the persisted `stats_hidden` preference is false, and a genuine toggle
//!   dispatches `set_stats_hidden` with the *rendered* checked value (checked
//!   means visible). Diagnostic logging dispatches the newly selected state.
//! - Both toggles are gated by a published snapshot and a non-closing
//!   session.
//! - Reset only asks the runtime for its existing confirmation decision; the
//!   token-checked dialog belongs to the mandatory-decision surface and is
//!   implemented nowhere in this file.
//! - Every dispatch re-checks the rendered popup identity and the current
//!   input/mandatory-dialog guards before handing the intent to
//!   `RuntimeCoordinator`; the returned `UiUpdate` goes through
//!   `HostState::consume`, so a projected refusal paints normally. An
//!   obsolete panel can neither toggle a replacement page nor request a
//!   reset.
//! - The disclosure is local UI state: it is never persisted, never implies a
//!   settings mutation, and does not change any projected fact.
//!
//! Host integration surface: the parent owns the settings popup panel, its
//! placement, scrolling, focus transfer and hit regions. [`PreferencesPage::new`]
//! creates the fixed focus handles, [`PreferencesPage::focus_targets`] feeds
//! the popup Tab chain in visual order, and [`paint`] renders the page body
//! for one rendered popup identity.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gpui::{
    AccessibleAction, App, ClickEvent, Context, Div, FocusHandle, Hsla, MouseButton,
    MouseDownEvent, Rgba, Role, SharedString, Stateful, Toggled, TransformationMatrix, WeakEntity,
    Window, canvas, div, prelude::*, px, radians,
};

use crate::ui::projection::Snapshot;

use super::{
    HostState, brume,
    chrome::Popup,
    companion_view::CompanionView,
    controls::{self, Activate, Activation, HitRegions},
    top_chrome::FocusTarget,
};

/// Stable element identities, so the accessible names stay put across paints.
pub(super) const ROOT_ID: &str = "preferences-root";
pub(super) const ATTENTION_ID: &str = "preferencesAttention";
pub(super) const STATS_ID: &str = "preferencesStatsHidden";
pub(super) const LOGGING_ID: &str = "preferencesLoggingEnabled";
pub(super) const RESET_ID: &str = "preferencesReset";
pub(super) const DETAILS_ID: &str = "preferencesTechnicalDetails";
pub(super) const STATUS_ID: &str = "preferencesStatus";
pub(super) const STARTUP_ID: &str = "preferencesStartup";
pub(super) const PATH_ID: &str = "preferencesPath";
pub(super) const IMMEDIATE_ID: &str = "preferencesImmediate";

/// The shared settings page width.
#[allow(
    dead_code,
    reason = "integration surface for the parent's settings panel sizing"
)]
pub(super) const WIDTH: f32 = brume::SETTINGS_WIDTH;

const LOGGING_NOTE: &str =
    "Write diagnostic logs and technical terminal output. Existing logs are kept when disabled.";
const RESET_NOTE: &str = "Clears saved source profiles and the last source. Keeps global preferences and the current capture.";
const DETAILS_LABEL: &str = "Technical details";

/// One runtime dispatch rendered by this page.
///
/// The value is captured at paint time. "Show stream statistics" is checked
/// while the published preference says visible, so a genuine toggle hides by
/// dispatching the rendered checked value as `hidden` (the next preference is
/// what the control already shows). Diagnostic logging dispatches the newly
/// selected state, and reset carries no token because the runtime owns the
/// pending decision identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Intent {
    SetStatsHidden { hidden: bool },
    SetLoggingEnabled { enabled: bool },
    RequestReset,
}

/// Facts the pure projection consumes, so the gates can be tested without a
/// live `HostState`.
struct PreferencesFacts<'a> {
    snapshot: Option<&'a Snapshot>,
    /// `ViewState::closing`, the host's own projected fact; the snapshot's
    /// `closing` is authoritative once one exists.
    closing: bool,
    /// `ViewState::stats_hidden`, the published persisted preference.
    stats_hidden: bool,
    /// `ViewState::logging_enabled`, the published persisted preference.
    logging_enabled: bool,
    /// `ViewState::fullscreen`, the persisted window preference.
    fullscreen: bool,
    /// Local disclosure state, never persisted.
    expanded: bool,
}

/// The preferences page's rendered model. Every field is derived from the
/// frame's facts; a missing snapshot renders an honest empty/disabled page
/// instead of a fabricated ready state.
struct PreferencesModel {
    attention: String,
    stats_checked: bool,
    stats_enabled: bool,
    logging_checked: bool,
    logging_enabled_control: bool,
    reset_enabled: bool,
    expanded: bool,
    status: String,
    startup_reason: String,
    path: String,
    /// `view.profile.live` plus the persisted window preference. `None` while
    /// no snapshot exists, so the page never invents volume or mute state.
    session: Option<String>,
}

impl PreferencesModel {
    fn project(facts: &PreferencesFacts<'_>) -> Self {
        let snapshot = facts.snapshot;
        let closing = facts.closing || snapshot.is_some_and(|view| view.closing);
        let editable = snapshot.is_some() && !closing;
        let settings = snapshot.map(|view| &view.settings);
        let path = settings
            .map(|settings| settings.path.clone())
            .unwrap_or_default();
        Self {
            attention: settings
                .map(|settings| settings.attention.clone())
                .unwrap_or_default(),
            // A genuine toggle dispatches `set_stats_hidden(!checked)`, so the
            // rendered checked value means visible.
            stats_checked: !facts.stats_hidden,
            stats_enabled: editable,
            logging_checked: facts.logging_enabled,
            logging_enabled_control: editable,
            // `!closing && settings.path.length > 0`.
            reset_enabled: !closing && !path.is_empty(),
            expanded: facts.expanded,
            status: settings
                .map(|settings| settings.status.clone())
                .unwrap_or_default(),
            startup_reason: settings
                .map(|settings| settings.startup_reason.clone())
                .unwrap_or_default(),
            path,
            session: snapshot.map(|view| {
                format!(
                    "Current session: volume {} % · mute {} · window {}",
                    view.live.volume_percent,
                    if view.live.muted {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    if facts.fullscreen {
                        "full screen"
                    } else {
                        "windowed"
                    }
                )
            }),
        }
    }

    fn from_host(state: &HostState, expanded: bool) -> Self {
        let facts = PreferencesFacts {
            snapshot: state.view.snapshot.as_deref(),
            closing: state.view.closing,
            stats_hidden: state.view.stats_hidden,
            logging_enabled: state.view.logging_enabled,
            fullscreen: state.view.fullscreen,
            expanded,
        };
        Self::project(&facts)
    }
}

impl HostState {
    /// Dispatch one preferences intent through the checked runtime intents.
    ///
    /// The surface must still be the rendered popup, the host must still
    /// accept input, and no mandatory decision may own the window; the
    /// rendered readiness/path gates are re-checked before the call. Valid but
    /// stale values still reach the runtime's own guard, whose refusal is
    /// projected through `consume`.
    fn preferences_dispatch(&mut self, popup: Popup, intent: Intent) -> bool {
        if self.chrome.popup() != Some(popup)
            || !self.accepts_input()
            || !self.view.close_dialog.is_empty()
        {
            return false;
        }
        let editable = self
            .view
            .snapshot
            .as_deref()
            .is_some_and(|view| !view.closing);
        let reset_ready = self
            .view
            .snapshot
            .as_deref()
            .is_some_and(|view| !view.closing && !view.settings.path.is_empty());
        match intent {
            Intent::SetStatsHidden { hidden } => {
                if !editable {
                    return false;
                }
                let update = self.runtime.set_stats_hidden(hidden);
                self.consume(update);
            }
            Intent::SetLoggingEnabled { enabled } => {
                if !editable {
                    return false;
                }
                let update = self.runtime.set_logging_enabled(enabled);
                self.consume(update);
            }
            Intent::RequestReset => {
                if !reset_ready {
                    return false;
                }
                let update = self.runtime.request_reset();
                self.consume(update);
            }
        }
        true
    }
}

/// Fixed focus handles plus the local disclosure bit owned by the parent
/// view, so repaints keep one GPUI focus node per control.
pub(super) struct PreferencesPage {
    stats: FocusHandle,
    logging: FocusHandle,
    reset: FocusHandle,
    details: FocusHandle,
    expanded: Rc<Cell<bool>>,
    /// Identity of the surface this page was last painted in. A replacement
    /// popup must not inherit the old page's disclosure state.
    surface: Cell<Option<Popup>>,
}

impl PreferencesPage {
    pub(super) fn new(cx: &mut Context<CompanionView>) -> Self {
        Self {
            stats: cx.focus_handle(),
            logging: cx.focus_handle(),
            reset: cx.focus_handle(),
            details: cx.focus_handle(),
            expanded: Rc::new(Cell::new(false)),
            surface: Cell::new(None),
        }
    }

    /// Integration hook for the parent: collapse the disclosure when the
    /// settings section is left or its popup closes. The local state is never
    /// persisted.
    #[allow(dead_code, reason = "integration surface for parent-owned navigation")]
    pub(super) fn reset_disclosure(&self) {
        self.expanded.set(false);
        self.surface.set(None);
    }

    /// The page's Tab chain in paint order, skipping disabled controls. The
    /// disclosure itself is never disabled.
    pub(super) fn focus_targets(&self, state: &HostState) -> Vec<FocusTarget> {
        let model = PreferencesModel::from_host(state, self.expanded.get());
        let mut targets = Vec::new();
        if model.stats_enabled {
            targets.push(FocusTarget::plain(&self.stats));
        }
        if model.logging_enabled_control {
            targets.push(FocusTarget::plain(&self.logging));
        }
        if model.reset_enabled {
            targets.push(FocusTarget::plain(&self.reset));
        }
        targets.push(FocusTarget::plain(&self.details));
        targets
    }
}

/// Render the Preferences page body for one rendered popup identity.
///
/// The caller places and scrolls the returned element inside the settings
/// panel; this function never maps, positions or focuses a window.
pub(super) fn paint(
    page: &PreferencesPage,
    state: &Rc<RefCell<HostState>>,
    popup: Popup,
    regions: &HitRegions,
    cx: &mut Context<CompanionView>,
) -> Stateful<Div> {
    if page.surface.replace(Some(popup)) != Some(popup) {
        // A different popup owns the surface: the previous page is gone, so
        // its local disclosure cannot carry over.
        page.expanded.set(false);
    }
    let expanded = page.expanded.get();
    let model = {
        let state = state.borrow();
        PreferencesModel::from_host(&state, expanded)
    };
    let weak = cx.weak_entity();

    let stats_activate = model.stats_enabled.then(|| {
        preferences_activation(
            state,
            weak.clone(),
            popup,
            Intent::SetStatsHidden {
                hidden: model.stats_checked,
            },
        )
    });
    let logging_activate = model.logging_enabled_control.then(|| {
        preferences_activation(
            state,
            weak.clone(),
            popup,
            Intent::SetLoggingEnabled {
                enabled: !model.logging_checked,
            },
        )
    });
    let reset_activate = model
        .reset_enabled
        .then(|| preferences_activation(state, weak.clone(), popup, Intent::RequestReset));
    let details_activate = disclosure_activation(state, weak, popup, Rc::clone(&page.expanded));

    let mut body = div()
        .id(ROOT_ID)
        .accessibility_id(ROOT_ID)
        .role(Role::Group)
        .aria_label("Preferences")
        .flex()
        .flex_col()
        .gap(px(brume::GAP_LARGE))
        .w_full();

    // Projected persistence attention: always outside the disclosure.
    if !model.attention.is_empty() {
        body = body.child(
            note_label(model.attention.clone(), Tone::Error)
                .id(ATTENTION_ID)
                .accessibility_id(ATTENTION_ID)
                .role(Role::Label)
                .aria_label(model.attention.clone())
                .w_full(),
        );
    }

    body = body.child(
        div()
            .flex()
            .flex_col()
            .gap(px(brume::GAP_SMALL))
            .w_full()
            .child(section_title("Display"))
            .child(toggle(
                STATS_ID,
                "Show stream statistics",
                model.stats_checked,
                model.stats_enabled,
                &page.stats,
                regions,
                stats_activate,
            )),
    );

    body = body.child(
        div()
            .flex()
            .flex_col()
            .gap(px(brume::GAP_SMALL))
            .w_full()
            .child(section_title("Diagnostics"))
            .child(toggle(
                LOGGING_ID,
                "Diagnostic logging",
                model.logging_checked,
                model.logging_enabled_control,
                &page.logging,
                regions,
                logging_activate,
            ))
            .child(note_label(LOGGING_NOTE, Tone::Note).w_full()),
    );

    body = body.child(
        div()
            .w_full()
            .h(px(brume::DIVIDER_WIDTH))
            .bg(brume::DIVIDER),
    );

    body = body.child(
        div()
            .flex()
            .flex_col()
            .gap(px(brume::GAP_SMALL))
            .w_full()
            .child(section_title("Saved settings"))
            .child(note_label(RESET_NOTE, Tone::Note).w_full())
            .child(
                controls::button(
                    RESET_ID,
                    "Reset saved file…",
                    None,
                    model.reset_enabled,
                    regions,
                    reset_activate,
                )
                .when(model.reset_enabled, |button| {
                    button.track_focus(&page.reset)
                })
                .aria_label("Reset the settings file with confirmation"),
            ),
    );

    body = body.child(disclosure_button(
        model.expanded,
        &page.details,
        regions,
        details_activate,
    ));

    if model.expanded {
        body = body.child(technical_details(&model));
    }

    body
}

/// Activation for one rendered toggle or the reset action: the popup identity
/// and the rendered value were captured at paint time. An accessibility
/// activation additionally requests chrome focus, like the other surfaces.
fn preferences_activation(
    state: &Rc<RefCell<HostState>>,
    view: WeakEntity<CompanionView>,
    popup: Popup,
    intent: Intent,
) -> Activate {
    let state = Rc::clone(state);
    Rc::new(move |source, _window, cx| {
        {
            let mut state = state.borrow_mut();
            if !state.preferences_dispatch(popup, intent) {
                return;
            }
            state.focus_intent |= source == Activation::Accessibility;
        }
        let _ = view.update(cx, |_, cx| cx.notify());
    })
}

/// The disclosure is local UI state, so its activation only checks that the
/// panel it was painted in still owns the surface; it never touches runtime
/// state.
fn disclosure_activation(
    state: &Rc<RefCell<HostState>>,
    view: WeakEntity<CompanionView>,
    popup: Popup,
    expanded: Rc<Cell<bool>>,
) -> Activate {
    let state = Rc::clone(state);
    Rc::new(move |_, _window, cx| {
        {
            let state = state.borrow();
            if state.chrome.popup() != Some(popup)
                || !state.accepts_input()
                || !state.view.close_dialog.is_empty()
            {
                return;
            }
            expanded.set(!expanded.get());
        }
        let _ = view.update(cx, |_, cx| cx.notify());
    })
}

/// Brume toggle: 20 px indicator, 36 px control height and the full row as hit
/// target. The focus ring is drawn on the row rather than the indicator because
/// GPUI focus styling applies to the pressed node; the accent ring and the
/// checked fill are the same tokens. The shared control module has no toggle
/// primitive yet; it should absorb this when its consumer slice lands.
fn toggle(
    id: &'static str,
    label_text: &'static str,
    checked: bool,
    enabled: bool,
    focus: &FocusHandle,
    regions: &HitRegions,
    activate: Option<Activate>,
) -> Stateful<Div> {
    let label_color = if enabled {
        brume::FOREGROUND
    } else {
        brume::DISABLED_TEXT
    };
    let indicator_fill = if !enabled {
        brume::DISABLED_FILL
    } else if checked {
        brume::ACCENT
    } else {
        brume::TRACK
    };
    let indicator_border = if checked {
        brume::ACCENT
    } else {
        brume::DIVIDER
    };

    let mut indicator = div()
        .w(px(20.))
        .h(px(20.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .border_1()
        .border_color(indicator_border)
        .bg(indicator_fill);
    if checked {
        indicator = indicator.child(glyph(brume::icon("check"), 14., brume::MATTE));
    }

    let mut row = div()
        .id(id)
        .accessibility_id(id)
        .relative()
        .w_full()
        .min_h(px(brume::CONTROL_HEIGHT))
        .flex()
        .items_center()
        .gap(px(brume::GAP))
        .border_2()
        .border_color(gpui::rgba(0x00000000))
        .rounded(px(brume::RADIUS_CONTROL))
        .role(Role::CheckBox)
        .aria_label(label_text)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .tab_stop(enabled)
        .child(indicator)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(px(brume::FONT_SIZE))
                .text_color(label_color)
                .child(SharedString::new_static(label_text)),
        );

    if enabled {
        let focus_for_press = focus.clone();
        let press = move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
            focus_for_press.focus(window, cx);
        };
        row = row
            .track_focus(focus)
            .cursor_pointer()
            .focus_visible(|style| style.border_color(brume::ACCENT))
            .on_mouse_down(MouseButton::Left, press)
            .child(hit_layer(regions));
        if let Some(activate) = activate {
            row = with_activation(row, activate);
        }
    }
    row
}

/// Disclosure button with the chevron as its trailing icon: sentence-case
/// label, 36 px box, visible focus ring and real hover/press.
fn disclosure_button(
    expanded: bool,
    focus: &FocusHandle,
    regions: &HitRegions,
    activate: Activate,
) -> Stateful<Div> {
    let description = if expanded {
        "Collapse technical details"
    } else {
        "Expand technical details"
    };
    with_activation(
        div()
            .id(DETAILS_ID)
            .accessibility_id(DETAILS_ID)
            .relative()
            .w_full()
            .h(px(brume::CONTROL_HEIGHT))
            .py(px(6.0))
            .px(px(brume::EDGE))
            .flex()
            .items_center()
            .gap(px(brume::GAP_SMALL))
            .rounded(px(brume::RADIUS_CONTROL))
            .border_1()
            .border_color(brume::DIVIDER)
            .bg(brume::CONTROL)
            .text_size(px(brume::FONT_SIZE))
            .text_color(brume::FOREGROUND)
            .role(Role::Button)
            .aria_label(DETAILS_LABEL)
            .aria_description(description)
            .aria_expanded(expanded)
            .tab_stop(true)
            .track_focus(focus)
            .focus_visible(|style| style.border_2().border_color(brume::ACCENT))
            .hover(|style| style.bg(brume::CONTROL_HOVER))
            .active(|style| style.bg(brume::CONTROL_PRESSED))
            .cursor_pointer()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(SharedString::new_static(DETAILS_LABEL)),
            )
            .child(glyph_flipped(
                brume::icon("chevron-down"),
                brume::ICON_SIZE,
                brume::FOREGROUND,
                expanded,
            ))
            .child(hit_layer(regions)),
        activate,
    )
}

/// The expanded disclosure body: persisted saved-settings facts, then the
/// current-session line. Labels are plain text; the path is never parsed.
fn technical_details(model: &PreferencesModel) -> Div {
    let mut body = div()
        .flex()
        .flex_col()
        .gap(px(brume::GAP_SMALL))
        .w_full()
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(0.))
                .w_full()
                .child(note_strong_label("Saved settings"))
                .child(
                    note_label(model.status.clone(), Tone::Note)
                        .id(STATUS_ID)
                        .accessibility_id(STATUS_ID)
                        .role(Role::Label)
                        .aria_label(model.status.clone())
                        .w_full(),
                ),
        );

    if !model.startup_reason.is_empty() {
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap(px(0.))
                .w_full()
                .child(note_strong_label("Startup restoration"))
                .child(
                    note_label(model.startup_reason.clone(), Tone::Note)
                        .id(STARTUP_ID)
                        .accessibility_id(STARTUP_ID)
                        .role(Role::Label)
                        .aria_label(model.startup_reason.clone())
                        .w_full(),
                ),
        );
    }

    if !model.path.is_empty() {
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap(px(0.))
                .w_full()
                .child(note_strong_label("Settings file"))
                .child(
                    note_label(model.path.clone(), Tone::Note)
                        .id(PATH_ID)
                        .accessibility_id(PATH_ID)
                        .role(Role::Label)
                        .aria_label(model.path.clone())
                        .w_full(),
                ),
        );
    }

    if let Some(session) = &model.session {
        body = body.child(
            note_label(session.clone(), Tone::Secondary)
                .id(IMMEDIATE_ID)
                .accessibility_id(IMMEDIATE_ID)
                .role(Role::Label)
                .aria_label(session.clone())
                .w_full(),
        );
    }

    body
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Note,
    Secondary,
    Error,
}

/// Plain-text Brume label in the note, secondary or error variant. GPUI text is
/// never interpreted as markup, so persistence status, paths and startup
/// reasons paint directly.
fn note_label(text: impl Into<SharedString>, tone: Tone) -> Div {
    let text: SharedString = text.into();
    let (size, color) = match tone {
        Tone::Note | Tone::Secondary => (brume::FONT_SIZE_SMALL, brume::SECONDARY),
        Tone::Error => (brume::FONT_SIZE, brume::ERROR),
    };
    div()
        .text_size(px(size))
        .font_weight(brume::FONT_WEIGHT_NORMAL)
        .text_color(color)
        .child(text)
}

fn note_strong_label(text: impl Into<SharedString>) -> Div {
    note_label(text, Tone::Note).font_weight(brume::FONT_WEIGHT_STRONG)
}

/// Body-size section heading in the strong note weight.
fn section_title(text: impl Into<SharedString>) -> Div {
    let text: SharedString = text.into();
    div()
        .w_full()
        .text_size(px(brume::FONT_SIZE))
        .font_weight(brume::FONT_WEIGHT_STRONG)
        .text_color(brume::FOREGROUND)
        .child(text)
}

/// One bundled SVG glyph painted at its stable asset path, like every other
/// Brume control (the pinned GPUI `Svg` element hashes and copies bytes per
/// frame). The optional 180° rotation is applied around the icon centre and
/// only affects painting, never layout or hit testing.
fn glyph(icon: Option<brume::Icon>, size: f32, tint: Rgba) -> impl IntoElement {
    glyph_flipped(icon, size, tint, false)
}

fn glyph_flipped(
    icon: Option<brume::Icon>,
    size: f32,
    tint: Rgba,
    flipped: bool,
) -> impl IntoElement {
    let color: Hsla = tint.into();
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            let Some(icon) = icon else {
                return;
            };
            let path = SharedString::new_static(icon.path);
            let mut transformation = TransformationMatrix::unit();
            if flipped {
                let center = bounds.center();
                let scale_factor = window.scale_factor();
                transformation = transformation
                    .translate(center.scale(scale_factor))
                    .rotate(radians(std::f32::consts::PI))
                    .translate(center.scale(-scale_factor));
            }
            if let Err(error) =
                window.paint_svg(bounds, path, Some(icon.bytes), transformation, color, cx)
            {
                tracing::warn!(event_code = "gpui_svg_paint", %error, path = icon.path);
            }
        },
    )
    .size(px(size))
}

/// Transparent full-bleed prepaint listener recording the rendered rectangle,
/// so the companion's X input region names the pixels GPUI committed.
fn hit_layer(regions: &HitRegions) -> impl IntoElement {
    let regions = regions.clone();
    canvas(move |bounds, _, _| regions.record(bounds), |_, _, _, _| {})
        .absolute()
        .inset_0()
}

/// Wire pointer/keyboard click and the AT-SPI `Click` action to one dispatch,
/// matching the shared control primitive.
fn with_activation(element: Stateful<Div>, activate: Activate) -> Stateful<Div> {
    let pointer = activate.clone();
    let accessibility = activate;
    element
        .on_click(move |event: &ClickEvent, window, cx| {
            let source = match event {
                ClickEvent::Keyboard(_) => Activation::Keyboard,
                ClickEvent::Mouse(_) | ClickEvent::Touch(_) => Activation::Pointer,
            };
            pointer(source, window, cx);
        })
        .on_a11y_action(
            AccessibleAction::Click,
            move |_: Option<&gpui::accesskit::ActionData>, window, cx| {
                accessibility(Activation::Accessibility, window, cx);
            },
        )
}

#[cfg(test)]
mod tests {
    use super::{PreferencesFacts, PreferencesModel};
    use crate::ui::projection::{
        Diagnostics, Live, Output, Recovery, SessionKind, Settings, Snapshot,
    };

    fn snapshot(closing: bool) -> Snapshot {
        Snapshot {
            schema: 2,
            session_kind: SessionKind::Capture,
            state_token: "state:1".into(),
            generation: "7".into(),
            product_phase: "Active".into(),
            gate_phase: "Ready".into(),
            paused: false,
            opening: false,
            closing,
            can_edit_selection: true,
            can_restart: false,
            can_reconnect: false,
            can_toggle_pause: true,
            can_set_gain: true,
            selection_revision: "1".into(),
            selection: None,
            applied: None,
            last_valid: None,
            live: Live {
                volume_percent: 65,
                muted: true,
                ..Default::default()
            },
            stream_stats: None,
            catalog: Default::default(),
            renderer: Default::default(),
            output: Output {
                revision: "1".into(),
                ..Default::default()
            },
            recovery: Recovery::default(),
            settings: Settings {
                status: "Saved".into(),
                attention: "Settings file refused".into(),
                path: "/tmp/furami/settings.json".into(),
                refused: true,
                startup_reason: "Restored the last source".into(),
                close_dialog: String::new(),
                reset_token: "9".into(),
            },
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

    fn facts<'a>(snapshot: Option<&'a Snapshot>) -> PreferencesFacts<'a> {
        PreferencesFacts {
            snapshot,
            closing: false,
            stats_hidden: false,
            logging_enabled: false,
            fullscreen: false,
            expanded: false,
        }
    }

    #[test]
    fn statistics_toggle_is_positive_and_dispatches_the_rendered_checked_state() {
        let snapshot = snapshot(false);
        let mut facts = facts(Some(&snapshot));
        // Published preference is "shown": the positive control is checked,
        // and a genuine toggle dispatches exactly the rendered checked value
        // (`hidden = true` to hide).
        let model = PreferencesModel::project(&facts);
        assert!(model.stats_checked);
        assert!(model.stats_enabled);

        // Hidden preference: the control renders unchecked and a genuine
        // toggle dispatches `hidden = false` (show), again the rendered value.
        facts.stats_hidden = true;
        let model = PreferencesModel::project(&facts);
        assert!(!model.stats_checked);
        assert!(model.stats_enabled);
    }

    #[test]
    fn preferences_controls_require_a_snapshot_and_a_non_closing_session() {
        let mut without = facts(None);
        without.stats_hidden = true;
        without.logging_enabled = true;
        let model = PreferencesModel::project(&without);
        assert!(!model.stats_enabled);
        assert!(!model.logging_enabled_control);
        assert!(!model.reset_enabled);
        assert!(model.session.is_none(), "no fabricated session values");
        assert!(model.attention.is_empty());
        assert!(model.path.is_empty());

        let closing = snapshot(true);
        let model = PreferencesModel::project(&facts(Some(&closing)));
        assert!(!model.stats_enabled);
        assert!(!model.logging_enabled_control);
        assert!(
            !model.reset_enabled,
            "closing disables reset even with a persisted path"
        );
    }

    #[test]
    fn attention_and_saved_settings_are_independent_of_the_disclosure() {
        let snapshot = snapshot(false);
        for expanded in [false, true] {
            let mut facts = facts(Some(&snapshot));
            facts.expanded = expanded;
            let model = PreferencesModel::project(&facts);
            assert_eq!(model.attention, "Settings file refused");
            assert_eq!(model.status, "Saved");
            assert_eq!(model.startup_reason, "Restored the last source");
            assert_eq!(model.path, "/tmp/furami/settings.json");
            assert_eq!(model.expanded, expanded);
        }
    }

    #[test]
    fn reset_is_enabled_only_with_a_persisted_path() {
        let mut empty = snapshot(false);
        empty.settings.path.clear();
        let model = PreferencesModel::project(&facts(Some(&empty)));
        assert!(!model.reset_enabled);

        let mut with_path = snapshot(false);
        with_path.settings.path = "/tmp/settings.json".into();
        let model = PreferencesModel::project(&facts(Some(&with_path)));
        assert!(model.reset_enabled);
    }

    #[test]
    fn session_line_matches_the_join() {
        let snapshot = snapshot(false);
        let model = PreferencesModel::project(&facts(Some(&snapshot)));
        assert_eq!(
            model.session.as_deref(),
            Some("Current session: volume 65 % · mute enabled · window windowed")
        );

        let mut facts = facts(Some(&snapshot));
        facts.fullscreen = true;
        let model = PreferencesModel::project(&facts);
        assert_eq!(
            model.session.as_deref(),
            Some("Current session: volume 65 % · mute enabled · window full screen")
        );
    }
}
