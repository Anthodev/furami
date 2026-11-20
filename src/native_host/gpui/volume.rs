//! Right-centre Brume volume rail and wheel policy for the GPUI frontend.
//!
//! `qml/VolumeChromeContent.qml` is the parity contract: a
//! `Brume.volumeWidth` (44 px) tool at the right window margin, vertically
//! centred, that grows from the 48 px ribbon extent to 216 logical px while the
//! pointer hovers it, it owns keyboard focus, or keyboard navigation expanded
//! it. The gain slider and the independent mute button step by one bounded
//! integer point, and neither keeps an optimistic local value: every change is
//! dispatched through `RuntimeCoordinator` and the rail paints only the
//! published projection.
//!
//! Host integration lives in `gpui.rs` and `companion_view.rs`; this module
//! only owns the presentation state, the render, and the wheel policy.
//!
//! - `HostState` carries one [`VolumeState`] (hover, keyboard expansion and the
//!   single signed wheel accumulator, so the video/empty viewport and the rail
//!   can never accumulate separately).
//! - Both window roots may attach `on_scroll`; they call
//!   [`HostState::volume_wheel`] with the same family-active/visibility fact the
//!   chrome uses, and rearm chrome activity only when the outcome is not
//!   [`WheelOutcome::Ignore`].
//! - The companion calls [`VolumeState::pointer`] beside `ChromeState::pointer`,
//!   [`VolumeState::keyboard_expand`] when Tab enters the volume tool, and
//!   [`VolumeState::reset`] whenever the chrome conceals, focus-family ownership
//!   is lost, or a popup boundary is crossed.
//! - [`rail`] is rendered inside the companion root (so the volume focus handles
//!   are contained by that root) only while the chrome is painted, the popup is
//!   closed and the session is not the qualification video. The parent adds the
//!   slider/mute [`FocusHandle`]s to its traversal between the ribbon and the
//!   bottom tool.

use std::{cell::Cell, rc::Rc};

use gpui::{
    AccessibleAction, App, Bounds, ClickEvent, Div, FocusHandle, HoverListenerMode, Hsla,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Orientation, Pixels,
    Point, Role, ScrollDelta, ScrollEvent, SharedString, Size, Stateful, TouchPhase,
    TransformationMatrix, Window, canvas, div, prelude::*, px,
};

use crate::app::ports::SubmitStatus;

use super::{
    HostState, brume,
    controls::{Activation, HitRegions},
};

/// Stable GPUI/accessibility identity of the gain slider.
pub(super) const SLIDER_ID: &str = "volume-slider";
/// Stable GPUI/accessibility identity of the mute button.
pub(super) const MUTE_ID: &str = "volume-mute";
/// Accessible name of the gain slider (the QML control's `Accessible.name`).
const SLIDER_LABEL: &str = "Playback volume";
/// Accessible name of the mute button (`VolumeChromeContent.qml` toggles it).
const MUTE_LABEL: &str = "Mute";
const UNMUTE_LABEL: &str = "Unmute";

/// Collapsed rail extent (`Brume.ribbonHeight`).
const RAIL_COLLAPSED: f32 = brume::RIBBON_HEIGHT;
/// Expanded rail extent (`VolumeChromeContent.qml`).
const RAIL_EXPANDED: f32 = 216.0;
/// Slider extent inside the rail (`implicitWidth - 8`, `implicitHeight`).
const SLIDER_WIDTH: f32 = brume::VOLUME_WIDTH - 8.0;
const SLIDER_HEIGHT: f32 = 140.0;
/// QML handle and undragged track widths.
const HANDLE: f32 = 18.0;
const TRACK_WIDTH: f32 = 6.0;

/// Bounds of the projected gain range.
const MIN_VOLUME: i32 = 0;
const MAX_VOLUME: i32 = 100;
/// One wheel detent is 5 percentage points (`notches * 5` in the QML adapter).
const POINTS_PER_NOTCH: i32 = 5;
/// Vertical lines in one detent from the pinned GPUI X11 backend: discrete
/// buttons and fractional XInput2 valuators both arrive scaled to `SCROLL_LINES`
/// = 3.0 lines per notch.
const LINES_PER_NOTCH: f32 = 3.0;
/// The accumulator works in thousandths of a line so fractional valuators add
/// up deterministically and a discrete detent divides exactly.
const MILLI_LINES_PER_NOTCH: i64 = (LINES_PER_NOTCH * 1000.0) as i64;

/// The runtime dispatch a rail control performs. The parent wires it to
/// [`HostState::apply_volume`], which re-checks the published guard.
pub(super) type Dispatch = Rc<dyn Fn(VolumeIntent, Activation, &mut Window, &mut App)>;

/// A gain change read from the rail. Every variant is bound to the values of
/// the frame that rendered it so a stale control cannot act on newer state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VolumeIntent {
    /// Set the slider to this absolute percentage (bounded by the runtime).
    Volume(i32),
    /// Toggle mute from the rendered projection: dispatch `!rendered` only
    /// while the projection still reports `rendered`.
    ToggleMute { rendered: bool },
}

/// What one wheel event did, so the host knows whether to rearm chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WheelOutcome {
    /// Not the volume rail's event: leave it to whatever is underneath.
    Ignore,
    /// Consumed as pointer activity; no gain change (sub-notch remainder).
    Consumed,
    /// Consumed; the runtime accepted this clamped target.
    Set(i32),
}

/// Rail interaction state. `hovered` only changes the painted extent and is
/// never treated as activity: a stationary pointer must still let the chrome
/// fade. `keyboard_expanded` mirrors the QML flag that survives until real
/// pointer activity clears keyboard navigation mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct VolumeState {
    hovered: bool,
    keyboard_expanded: bool,
    wheel: WheelAccumulator,
}

impl VolumeState {
    /// Whether the rail paints the 216 px extent.
    pub(super) fn expanded(&self, focused: bool) -> bool {
        self.hovered || focused || self.keyboard_expanded
    }

    /// Record a hover transition. Returns whether the extent changed, so the
    /// caller can avoid a needless repaint.
    pub(super) fn hover(&mut self, hovered: bool) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        true
    }

    /// Tab entered the volume tool: keep it expanded while keyboard navigation
    /// continues, exactly like `VolumeChromeContent.qml::focusBoundary`.
    pub(super) fn keyboard_expand(&mut self) {
        self.keyboard_expanded = true;
    }

    /// Real pointer motion ended keyboard navigation mode. Hover is owned by the
    /// rail's own hover listener and is not touched here.
    pub(super) fn pointer(&mut self) {
        self.keyboard_expanded = false;
    }

    /// Concealment or focus-family loss: no rail is on screen, so nothing may
    /// stay expanded and no sub-notch remainder may survive the boundary.
    pub(super) fn reset(&mut self) {
        self.hovered = false;
        self.keyboard_expanded = false;
        self.wheel.reset();
    }
}

/// One signed wheel accumulator shared by the video/empty viewport and the rail.
///
/// The pinned backend reports `ScrollDelta::Lines`, positive towards the top of
/// the screen, at three lines per detent. Values are accumulated as thousandths
/// of a line and only whole signed notches are consumed, so one physical detent
/// (or six 0.5-line smooth events) produces exactly one 5-point step, with no
/// device-pixel-ratio scaling and no extra sign reversal. Pixel deltas and
/// horizontal-only events are not volume input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct WheelAccumulator {
    milli_lines: i64,
}

impl WheelAccumulator {
    /// Clear the sub-notch remainder. Call it on popup, focus, eligibility and
    /// phase boundaries so a fraction from one context cannot combine with
    /// another.
    pub(super) fn reset(&mut self) {
        self.milli_lines = 0;
    }

    /// Consume one vertical event. `eligible` is the same full guard the
    /// controls use: active/shown app, no popup, not closing, published gain
    /// available and no modifiers.
    fn apply(&mut self, event: &ScrollEvent, eligible: bool, volume_percent: i32) -> WheelOutcome {
        if !eligible || event.modifiers.modified() {
            self.reset();
            return WheelOutcome::Ignore;
        }
        let Some(lines) = vertical_lines(&event.delta) else {
            // Pixel deltas and horizontal-only events are not volume input; the
            // remainder they did not produce stays for the next vertical event.
            return WheelOutcome::Ignore;
        };
        if event.touch_phase == TouchPhase::Started {
            self.reset();
        }
        let milli = (f64::from(lines) * 1000.0).round() as i64;
        let total = self.milli_lines.saturating_add(milli);
        let notches = total / MILLI_LINES_PER_NOTCH;
        self.milli_lines = total % MILLI_LINES_PER_NOTCH;

        let current = volume_percent.clamp(MIN_VOLUME, MAX_VOLUME);
        let target = (i64::from(current) + notches * i64::from(POINTS_PER_NOTCH))
            .clamp(i64::from(MIN_VOLUME), i64::from(MAX_VOLUME)) as i32;
        // Outward saturation and the end of a gesture discard the remainder so
        // the next detent starts a fresh step instead of finishing a stale one.
        if (milli > 0 && target == MAX_VOLUME)
            || (milli < 0 && target == MIN_VOLUME)
            || matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled)
        {
            self.reset();
        }
        if target == current {
            WheelOutcome::Consumed
        } else {
            WheelOutcome::Set(target)
        }
    }
}

/// Vertical lines carried by one event, or `None` when it is not a vertical
/// lines delta. Horizontal-only lines and every pixel delta are ignored.
fn vertical_lines(delta: &ScrollDelta) -> Option<f32> {
    match delta {
        ScrollDelta::Lines(point) if point.y != 0.0 => Some(point.y),
        ScrollDelta::Lines(_) | ScrollDelta::Pixels(_) => None,
    }
}

impl HostState {
    /// The single wheel entry point for the video/empty viewport and the rail.
    ///
    /// `active` is the host's family-active and chrome-visible fact; the rest of
    /// the gain guard is read from the published projection here so both windows
    /// cannot drift. A consumed outcome re-arms chrome activity, an ignored one
    /// leaves regular list scrolling alone.
    pub(super) fn volume_wheel(&mut self, event: &ScrollEvent, active: bool) -> WheelOutcome {
        let snapshot = self.view.snapshot.as_deref();
        let eligible = active
            && self.accepts_input()
            && self.chrome.popup().is_none()
            && snapshot.is_some_and(|snapshot| {
                snapshot.can_set_gain && !snapshot.closing && !snapshot.opening
            });
        let volume = snapshot.map_or(MIN_VOLUME, |snapshot| snapshot.live.volume_percent);
        let outcome = self.volume.wheel.apply(event, eligible, volume);
        if let WheelOutcome::Set(percent) = outcome {
            self.apply_volume(VolumeIntent::Volume(percent));
        }
        outcome
    }

    /// Dispatch one gain change through the runtime and consume the resulting
    /// projection. This is the only place the rail touches persisted state; the
    /// slider never paints a value the runtime did not publish.
    pub(super) fn apply_volume(&mut self, intent: VolumeIntent) -> bool {
        if !self.accepts_input() || self.chrome.popup().is_some() {
            return false;
        }
        let Some(snapshot) = self.view.snapshot.as_deref() else {
            return false;
        };
        if !snapshot.can_set_gain {
            return false;
        }
        let accepted = match intent {
            VolumeIntent::Volume(percent) => {
                self.runtime
                    .set_volume(percent.clamp(MIN_VOLUME, MAX_VOLUME))
                    == SubmitStatus::Accepted
            }
            VolumeIntent::ToggleMute { rendered } => {
                if snapshot.live.muted != rendered {
                    return false;
                }
                self.runtime.set_muted(!rendered) == SubmitStatus::Accepted
            }
        };
        let update = self.runtime.poll();
        self.consume(update);
        accepted
    }
}

/// Everything one rail frame needs from the projection and the companion.
pub(super) struct Rail {
    /// Companion viewport in logical px; the rail is vertically centred in it.
    pub(super) viewport: Size<Pixels>,
    /// Hover/focus/keyboard expansion, from [`VolumeState::expanded`].
    pub(super) expanded: bool,
    /// Published `can_set_gain`: both controls become visible-but-disabled.
    pub(super) enabled: bool,
    /// Slider holds keyboard-visible focus (`Control.visualFocus`).
    pub(super) visual_focus: bool,
    pub(super) volume_percent: i32,
    pub(super) muted: bool,
}

/// A rail hover transition, called with the new hovered fact.
pub(super) type Hover = Rc<dyn Fn(bool, &mut Window, &mut App)>;

/// Paint the right-centre rail and wire its pointer, keyboard and accessibility
/// actions. The caller must only invoke this while the companion is painted with
/// no open popup and outside the qualification video, and must place it inside
/// the companion root that tracks the view's root focus handle.
pub(super) fn rail(
    rail: Rail,
    slider_focus: &FocusHandle,
    mute_focus: &FocusHandle,
    regions: &HitRegions,
    hover: Hover,
    dispatch: Dispatch,
) -> impl IntoElement {
    let height = if rail.expanded {
        RAIL_EXPANDED
    } else {
        RAIL_COLLAPSED
    };
    let top = ((f32::from(rail.viewport.height) - height) / 2.0).max(0.0);

    div()
        .id("volume-rail")
        .absolute()
        .right(px(brume::WINDOW_MARGIN))
        .top(px(top))
        .w(px(brume::VOLUME_WIDTH))
        .h(px(height))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(brume::GAP_SMALL))
        .on_hover(move |hovered, window, cx| hover(*hovered, window, cx))
        // Hover is the physical pointer position, like the QML HoverHandler:
        // keyboard modality must not collapse a rail the pointer still covers.
        .hover_listener_mode(HoverListenerMode::InputModalityIndependent)
        .when(rail.expanded, |element| {
            element.child(slider(
                rail.volume_percent,
                rail.enabled,
                rail.visual_focus,
                slider_focus,
                dispatch.clone(),
            ))
        })
        .child(mute_button(rail.muted, rail.enabled, mute_focus, dispatch))
        .child(hit_area(regions))
}

/// Vertical gain slider: 36 x 140 track, filled below the 18 px handle, painted
/// only from the published percentage. Pressing jumps to the pressed position
/// and dragging follows it, both dispatching bounded integer steps.
fn slider(
    percent: i32,
    enabled: bool,
    visual_focus: bool,
    focus: &FocusHandle,
    dispatch: Dispatch,
) -> impl IntoElement {
    let percent = percent.clamp(MIN_VOLUME, MAX_VOLUME);
    let value = percent as f32 / 100.0;
    let bounds: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
    let pressed = Rc::new(Cell::new(false));

    let track_left = (SLIDER_WIDTH - TRACK_WIDTH) / 2.0;
    let track = div()
        .absolute()
        .left(px(track_left))
        .top_0()
        .w(px(TRACK_WIDTH))
        .h(px(SLIDER_HEIGHT))
        .rounded(px(3.0))
        .bg(brume::TRACK);
    let fill = div()
        .absolute()
        .left(px(track_left))
        .top(px((1.0 - value) * SLIDER_HEIGHT))
        .w(px(TRACK_WIDTH))
        .h(px(value * SLIDER_HEIGHT))
        .rounded(px(3.0))
        .bg(if enabled {
            brume::TRACK_FILL
        } else {
            brume::DISABLED_TEXT
        });
    let handle = div()
        .absolute()
        .left(px((SLIDER_WIDTH - HANDLE) / 2.0))
        .top(px((1.0 - value) * (SLIDER_HEIGHT - HANDLE)))
        .size(px(HANDLE))
        .rounded_full()
        .bg(if enabled {
            brume::FOREGROUND
        } else {
            brume::DISABLED_TEXT
        })
        .when(visual_focus, |element| {
            element.border_2().border_color(brume::ACCENT)
        });

    let mut element = div()
        .id(SLIDER_ID)
        .accessibility_id(SLIDER_ID)
        .relative()
        .w(px(SLIDER_WIDTH))
        .h(px(SLIDER_HEIGHT))
        .role(Role::Slider)
        .aria_label(SLIDER_LABEL)
        .aria_orientation(Orientation::Vertical)
        .aria_min_numeric_value(f64::from(MIN_VOLUME))
        .aria_max_numeric_value(f64::from(MAX_VOLUME))
        .aria_numeric_value_step(1.0)
        .aria_numeric_value(f64::from(percent))
        .tab_stop(enabled)
        .track_focus(focus);

    if enabled {
        let press = {
            let bounds = Rc::clone(&bounds);
            let pressed = Rc::clone(&pressed);
            let focus = focus.clone();
            let dispatch = dispatch.clone();
            move |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                focus.focus(window, cx);
                pressed.set(true);
                if let Some(bounds) = bounds.get() {
                    dispatch(
                        VolumeIntent::Volume(value_at(&bounds, event.position)),
                        Activation::Pointer,
                        window,
                        cx,
                    );
                }
            }
        };
        let drag = {
            let bounds = Rc::clone(&bounds);
            let pressed = Rc::clone(&pressed);
            let dispatch = dispatch.clone();
            move |event: &MouseMoveEvent, window: &mut Window, cx: &mut App| {
                if !pressed.get() || event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                if let Some(bounds) = bounds.get() {
                    dispatch(
                        VolumeIntent::Volume(value_at(&bounds, event.position)),
                        Activation::Pointer,
                        window,
                        cx,
                    );
                }
            }
        };
        let release = {
            let pressed = Rc::clone(&pressed);
            move |_: &MouseUpEvent, _: &mut Window, _: &mut App| pressed.set(false)
        };
        let keys = {
            let dispatch = dispatch.clone();
            move |event: &KeyDownEvent, window: &mut Window, cx: &mut App| {
                if event.keystroke.modifiers.modified() {
                    return;
                }
                // Vertical orientation: only Up/Down move the handle, matching
                // `QQuickSlider::keyPressEvent` (stepSize 1). Keys repeat.
                let step = match event.keystroke.key.as_str() {
                    "up" => 1,
                    "down" => -1,
                    _ => return,
                };
                let target = (percent + step).clamp(MIN_VOLUME, MAX_VOLUME);
                if target != percent {
                    dispatch(
                        VolumeIntent::Volume(target),
                        Activation::Keyboard,
                        window,
                        cx,
                    );
                }
            }
        };
        element = element
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, press)
            .on_mouse_move(drag)
            .on_mouse_up(MouseButton::Left, release.clone())
            .on_mouse_up_out(MouseButton::Left, release)
            .on_key_down(keys);
        let increment = {
            let dispatch = dispatch.clone();
            move |_: Option<&gpui::accesskit::ActionData>, window: &mut Window, cx: &mut App| {
                let target = (percent + 1).clamp(MIN_VOLUME, MAX_VOLUME);
                if target != percent {
                    dispatch(
                        VolumeIntent::Volume(target),
                        Activation::Accessibility,
                        window,
                        cx,
                    );
                }
            }
        };
        let decrement = {
            let dispatch = dispatch.clone();
            move |_: Option<&gpui::accesskit::ActionData>, window: &mut Window, cx: &mut App| {
                let target = (percent - 1).clamp(MIN_VOLUME, MAX_VOLUME);
                if target != percent {
                    dispatch(
                        VolumeIntent::Volume(target),
                        Activation::Accessibility,
                        window,
                        cx,
                    );
                }
            }
        };
        element = element
            .on_a11y_action(AccessibleAction::Increment, increment)
            .on_a11y_action(AccessibleAction::Decrement, decrement);
    }

    element
        .child(track)
        .child(fill)
        .child(handle)
        .child(measure(bounds))
}

/// Independent mute toggle: 44 x 48 rounded control (the tool is 44 px wide, so
/// that is the real QML hit target) with the 20 px Tabler glyph.
fn mute_button(
    muted: bool,
    enabled: bool,
    focus: &FocusHandle,
    dispatch: Dispatch,
) -> Stateful<Div> {
    let label = if muted { UNMUTE_LABEL } else { MUTE_LABEL };
    let icon = brume::icon(if muted { "volume-off" } else { "volume" });
    let tint = if enabled {
        brume::SECONDARY
    } else {
        brume::DISABLED_TEXT
    };
    let area: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));

    let mut element = div()
        .id(MUTE_ID)
        .accessibility_id(MUTE_ID)
        .relative()
        .w(px(brume::VOLUME_WIDTH))
        .h(px(brume::RIBBON_HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(brume::RADIUS_CONTROL))
        .border_2()
        .border_color(gpui::rgba(0x00000000))
        .role(Role::Button)
        .aria_label(label)
        .tab_stop(enabled)
        .track_focus(focus);

    if let Some(icon) = icon {
        element = element.child(glyph(icon, brume::RIBBON_ICON_SIZE, tint, area.clone()));
    }
    element = element.child(measure(area));

    if enabled {
        let activate = {
            let dispatch = dispatch.clone();
            move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                let source = match event {
                    ClickEvent::Keyboard(_) => Activation::Keyboard,
                    ClickEvent::Mouse(_) | ClickEvent::Touch(_) => Activation::Pointer,
                };
                dispatch(
                    VolumeIntent::ToggleMute { rendered: muted },
                    source,
                    window,
                    cx,
                );
            }
        };
        let accessibility = {
            let dispatch = dispatch.clone();
            move |_: Option<&gpui::accesskit::ActionData>, window: &mut Window, cx: &mut App| {
                dispatch(
                    VolumeIntent::ToggleMute { rendered: muted },
                    Activation::Accessibility,
                    window,
                    cx,
                );
            }
        };
        let press = {
            let focus = focus.clone();
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                focus.focus(window, cx);
            }
        };
        element = element
            .cursor_pointer()
            .hover(|style| style.bg(brume::CONTROL_HOVER))
            .active(|style| style.bg(brume::CONTROL_PRESSED))
            .focus_visible(|style| style.border_color(brume::ACCENT))
            .on_mouse_down(MouseButton::Left, press)
            .on_click(activate)
            .on_a11y_action(AccessibleAction::Click, accessibility);
    }
    element
}

/// One bundled SVG glyph painted at its stable asset path, exactly like the gate
/// controls. The tint follows `BrumeIconButton.qml`: secondary at rest, the
/// foreground while the pointer is over the 44 x 48 affordance (not merely the
/// 20 px glyph), disabled text when the gain is not settable.
fn glyph(
    icon: brume::Icon,
    size: f32,
    rest: gpui::Rgba,
    area: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> impl IntoElement {
    let path = SharedString::new_static(icon.path);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            let hovered = area
                .get()
                .is_some_and(|area| area.contains(&window.mouse_position()));
            let tint = if hovered { brume::FOREGROUND } else { rest };
            let color: Hsla = tint.into();
            if let Err(error) = window.paint_svg(
                bounds,
                path,
                Some(icon.bytes),
                TransformationMatrix::unit(),
                color,
                cx,
            ) {
                tracing::warn!(event_code = "gpui_svg_paint", %error, path = icon.path);
            }
        },
    )
    .size(px(size))
}

/// Mandatory rail input hole: the whole tool extent, like the QML tool window.
/// The column is transparent, but presses and hover inside it still belong to
/// the volume surface rather than the video child.
fn hit_area(regions: &HitRegions) -> impl IntoElement {
    let regions = regions.clone();
    canvas(move |bounds, _, _| regions.record(bounds), |_, _, _, _| {})
        .absolute()
        .inset_0()
}

/// Record the slider's painted bounds so pointer positions map to values in the
/// same coordinate space GPUI delivered them in.
fn measure(cell: Rc<Cell<Option<Bounds<Pixels>>>>) -> impl IntoElement {
    canvas(|_, _, _| (), move |bounds, (), _, _| cell.set(Some(bounds)))
        .absolute()
        .inset_0()
}

/// Map a window position to the bounded integer percentage the QML slider would
/// report for the same pointer position: the vertical visual position is
/// inverted, and `stepSize: 1` rounds to whole points.
fn value_at(bounds: &Bounds<Pixels>, position: Point<Pixels>) -> i32 {
    let y = f32::from(position.y - bounds.origin.y).clamp(0.0, SLIDER_HEIGHT);
    let visual = 1.0 - y / SLIDER_HEIGHT;
    (visual * 100.0)
        .round()
        .clamp(MIN_VOLUME as f32, MAX_VOLUME as f32) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, ScrollSource, point, size};

    fn event(delta: ScrollDelta) -> ScrollEvent {
        ScrollEvent {
            position: point(px(0.), px(0.)),
            delta,
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        }
    }

    fn lines(x: f32, y: f32) -> ScrollEvent {
        event(ScrollDelta::Lines(point(x, y)))
    }

    fn logged(
        accumulator: &mut WheelAccumulator,
        event: &ScrollEvent,
        volume: i32,
    ) -> WheelOutcome {
        accumulator.apply(event, true, volume)
    }

    #[test]
    fn gpui_host_volume_wheel_one_detent_steps_five_points() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 3.0), 50),
            WheelOutcome::Set(55)
        );
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 3.0), 55),
            WheelOutcome::Set(60)
        );
        // Negative lines scroll down and are not sign-flipped again.
        assert_eq!(
            logged(&mut wheel, &lines(0.0, -3.0), 60),
            WheelOutcome::Set(55)
        );
        // Multiple detents in one event stay proportional.
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 9.0), 55),
            WheelOutcome::Set(70)
        );
    }

    #[test]
    fn gpui_host_volume_wheel_consumes_only_whole_notches() {
        let mut wheel = WheelAccumulator::default();
        // Six smooth 0.5-line events make one 3-line detent; the rest is retained.
        for _ in 0..5 {
            assert_eq!(
                logged(&mut wheel, &lines(0.0, 0.5), 40),
                WheelOutcome::Consumed
            );
        }
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 0.5), 40),
            WheelOutcome::Set(45)
        );
        // Fractional valuators are not scaled by device pixel ratio or the rail
        // size: the next 0.5 lines is only a third of a notch.
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 0.5), 45),
            WheelOutcome::Consumed
        );
    }

    #[test]
    fn gpui_host_volume_wheel_ignores_pixels_and_horizontal_only() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 2.0), 50),
            WheelOutcome::Consumed
        );
        assert_eq!(
            logged(
                &mut wheel,
                &event(ScrollDelta::Pixels(point(px(0.), px(-30.)))),
                50
            ),
            WheelOutcome::Ignore
        );
        assert_eq!(
            logged(&mut wheel, &lines(4.0, 0.0), 50),
            WheelOutcome::Ignore
        );
        // Neither ignored event cleared the legitimate remainder.
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 1.0), 50),
            WheelOutcome::Set(55)
        );
    }

    #[test]
    fn gpui_host_volume_wheel_saturates_and_clears_outward_remainder() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 3.0), 98),
            WheelOutcome::Set(100)
        );
        // Remainder was cleared at the outward boundary: 1 line is a third.
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 1.0), 100),
            WheelOutcome::Consumed
        );
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 2.0), 100),
            WheelOutcome::Consumed
        );
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 2.0), 0),
            WheelOutcome::Consumed
        );
        assert_eq!(
            logged(&mut wheel, &lines(0.0, 1.0), 0),
            WheelOutcome::Set(5)
        );
        assert_eq!(
            logged(&mut wheel, &lines(0.0, -2.0), 5),
            WheelOutcome::Consumed
        );
        assert_eq!(
            logged(&mut wheel, &lines(0.0, -2.0), 5),
            WheelOutcome::Set(0)
        );
        // The outward boundary cleared the negative remainder as well.
        assert_eq!(
            logged(&mut wheel, &lines(0.0, -2.0), 0),
            WheelOutcome::Consumed
        );
    }

    #[test]
    fn gpui_host_volume_wheel_ignores_ineligible_and_modified_events() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(
            wheel.apply(&lines(0.0, 2.0), false, 50),
            WheelOutcome::Ignore
        );
        assert_eq!(
            wheel.apply(&lines(0.0, 1.0), true, 50),
            WheelOutcome::Consumed
        );
        assert_eq!(
            wheel.apply(&lines(0.0, 2.0), true, 50),
            WheelOutcome::Set(55)
        );

        let mut modified = event(ScrollDelta::Lines(point(0.0, 3.0)));
        modified.modifiers.shift = true;
        assert_eq!(wheel.apply(&modified, true, 55), WheelOutcome::Ignore);
        // The refused event cleared the remainder instead of resuming it.
        assert_eq!(
            wheel.apply(&lines(0.0, 1.0), true, 55),
            WheelOutcome::Consumed
        );
    }

    #[test]
    fn gpui_host_volume_wheel_gesture_boundaries_reset_remainder() {
        let mut wheel = WheelAccumulator::default();
        assert_eq!(
            wheel.apply(&lines(0.0, 2.0), true, 50),
            WheelOutcome::Consumed
        );
        let mut started = lines(0.0, 2.0);
        started.touch_phase = TouchPhase::Started;
        assert_eq!(wheel.apply(&started, true, 50), WheelOutcome::Consumed);
        // Started dropped the earlier remainder; 1 line completes the two lines
        // accumulated by the Started event itself.
        assert_eq!(
            wheel.apply(&lines(0.0, 1.0), true, 50),
            WheelOutcome::Set(55)
        );
        let mut ended = lines(0.0, 1.0);
        ended.touch_phase = TouchPhase::Ended;
        assert_eq!(wheel.apply(&ended, true, 55), WheelOutcome::Consumed);
        // Ended discarded the remainder after the event it terminated.
        assert_eq!(
            wheel.apply(&lines(0.0, 1.0), true, 55),
            WheelOutcome::Consumed
        );
    }

    #[test]
    fn gpui_host_volume_state_expands_on_hover_focus_and_keyboard() {
        let mut volume = VolumeState::default();
        assert!(!volume.expanded(false));
        assert!(volume.hover(true));
        assert!(!volume.hover(true));
        assert!(volume.expanded(false));
        assert!(volume.expanded(true));
        volume.pointer();
        assert!(volume.hovered);
        volume.hover(false);
        assert!(volume.expanded(true));
        volume.keyboard_expand();
        assert!(volume.expanded(false));
        volume.pointer();
        assert!(!volume.expanded(false));
        volume.reset();
        // reset() is for concealment/family loss; the caller no longer renders
        // a rail, so only the stored state (not a hypothetical focused=true
        // the caller would never pass) is observable here.
        assert!(!volume.expanded(false));
    }

    #[test]
    fn gpui_host_volume_slider_maps_vertical_position_to_integer_percent() {
        let bounds = Bounds {
            origin: point(px(10.), px(20.)),
            size: size(px(SLIDER_WIDTH), px(SLIDER_HEIGHT)),
        };
        assert_eq!(value_at(&bounds, point(px(28.), px(20.))), 100);
        assert_eq!(value_at(&bounds, point(px(28.), px(90.))), 50);
        assert_eq!(value_at(&bounds, point(px(28.), px(160.))), 0);
        // Out-of-bounds drags clamp instead of escaping the range.
        assert_eq!(value_at(&bounds, point(px(28.), px(-100.))), 100);
        assert_eq!(value_at(&bounds, point(px(28.), px(400.))), 0);
    }
}
