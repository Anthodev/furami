//! Shared Brume controls for the GPUI frontend.
//!
//! Scoped to what the native-composition gate actually renders: a title
//! label and one text/icon button primitive shared by Pause/Resume, Options
//! and the qualification close action. Rows, choices, toggles, sliders, scroll
//! views, fields and popups arrive with the real views that consume them.
//!
//! Every control is layout-neutral: it sizes itself from the Brume tokens and
//! the caller may further position it (`.absolute().left(...)`, `.w(...)`) on
//! the returned element. A control that is enabled paints a transparent,
//! full-bleed canvas whose prepaint records the *rendered* rectangle into a
//! [`HitRegions`], so the companion's X input region names the pixels GPUI
//! actually committed rather than authored guesses.
//!
//! Focus is deliberately not self-managed: the caller applies
//! `.track_focus(&handle)` so it owns the [`gpui::FocusHandle`] it needs to
//! restore focus after a popup dismisses or an editor changes identity. The
//! fixed element ids keep the same GPUI focus node across Pause↔Resume.

use std::{cell::RefCell, rc::Rc};

use gpui::{
    AccessibleAction, App, Bounds, ClickEvent, Div, Hsla, Pixels, Rgba, Role, SharedString,
    Stateful, TransformationMatrix, Window, canvas, div, prelude::*, px,
};

use super::brume;

/// Human FPS label for the exact projected `N` or `N/D` (QML `rateFps`).
/// This approximation is presentation-only; intents use the original text.
pub(super) fn rate_fps(exact: &str) -> String {
    let (numerator, denominator) = exact.split_once('/').unwrap_or((exact, "1"));
    let parse = |value: &str| {
        if value.bytes().all(|byte| byte.is_ascii_digit()) {
            value.parse::<u64>().ok()
        } else {
            None
        }
    };
    let (Some(numerator), Some(denominator)) = (parse(numerator), parse(denominator)) else {
        return exact.to_owned();
    };
    // Match the QML safe integer bound before converting to float.
    const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
    if denominator == 0 || numerator > MAX_SAFE_INTEGER || denominator > MAX_SAFE_INTEGER {
        return exact.to_owned();
    }
    if numerator.is_multiple_of(denominator) {
        return format!("{} FPS", numerator / denominator);
    }
    let rounded = format!("{:.2}", numerator as f64 / denominator as f64);
    format!(
        "{} FPS",
        rounded.trim_end_matches('0').trim_end_matches('.')
    )
}

/// Why a control was activated. `Pointer` and `Keyboard` come from GPUI's own
/// synthesized click; `Accessibility` comes from an AT-SPI `Click` action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Activation {
    Pointer,
    Keyboard,
    Accessibility,
}

/// The runtime dispatch a control performs. It must contain the same guard the
/// QML action uses; controls never touch host state themselves.
pub(super) type Activate = Rc<dyn Fn(Activation, &mut Window, &mut App)>;

/// Stable identity of the gate Pause/Resume (or Open) control.
pub(super) const PAUSE_RESUME_ID: &str = "gate-pause-resume";
/// Stable identity of the gate Options control.
pub(super) const OPTIONS_ID: &str = "gate-options";

/// Frame-scoped collection of rendered hit rectangles, in logical window
/// pixels.
///
/// The host creates a fresh collector for each render, passes it to the gate
/// controls, and reads [`HitRegions::rects`] once that frame is prepainted.
/// Only enabled controls record; a disabled control is not an input hole, so
/// presses over it reach whatever is underneath (the video child or the
/// companion's empty input region).
#[derive(Clone, Default)]
pub(super) struct HitRegions {
    rects: Rc<RefCell<Vec<Bounds<Pixels>>>>,
}

impl HitRegions {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Append one rendered rectangle. Exposed to the host module so a
    /// composition layer can add its own regions (for example a modal
    /// backdrop) to the same union.
    pub(super) fn record(&self, bounds: Bounds<Pixels>) {
        let mut rects = self.rects.borrow_mut();
        // A control may prepaint twice in one frame; identical rectangles add
        // no information to the union.
        if rects.last() != Some(&bounds) {
            rects.push(bounds);
        }
    }

    /// The last prepainted rectangles, in paint order.
    pub(super) fn rects(&self) -> Vec<Bounds<Pixels>> {
        self.rects.borrow().clone()
    }
}

/// Brume label variants. Body, secondary, note and error arrive with the
/// views that consume them; the gate opens with the dialog title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LabelVariant {
    Title,
}

/// A plain-text Brume label. GPUI text is never interpreted as markup, so
/// device names, diagnostics and recovery text can be painted directly.
pub(super) fn label(text: impl Into<SharedString>, variant: LabelVariant) -> Div {
    let text: SharedString = text.into();
    let (size, weight, color) = match variant {
        LabelVariant::Title => (
            brume::FONT_SIZE_TITLE,
            brume::FONT_WEIGHT_STRONG,
            brume::FOREGROUND,
        ),
    };
    div()
        .text_size(px(size))
        .font_weight(weight)
        .text_color(color)
        .child(text)
}

/// Themed text button: sentence-case label with an optional leading glyph,
/// 36 px high, visible keyboard focus ring and a real hover/press state.
///
/// The caller must apply `.track_focus(&handle)`; the control is focusable but
/// is not itself a tab index owner. When `enabled` is false there is no tab
/// stop, no click handler and no accessible action.
pub(super) fn button(
    id: &'static str,
    label: impl Into<SharedString>,
    icon: Option<brume::Icon>,
    enabled: bool,
    regions: &HitRegions,
    activate: Option<Activate>,
) -> Stateful<Div> {
    let label: SharedString = label.into();
    button_frame(id, label.clone(), icon, enabled, regions, activate).child(label)
}

/// The same action control with caller-owned visual content.
pub(super) fn button_frame(
    id: &'static str,
    accessible_name: impl Into<SharedString>,
    icon: Option<brume::Icon>,
    enabled: bool,
    regions: &HitRegions,
    activate: Option<Activate>,
) -> Stateful<Div> {
    let tint = if enabled {
        brume::FOREGROUND
    } else {
        brume::DISABLED_TEXT
    };
    let mut element = div()
        .id(id)
        .accessibility_id(id)
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(brume::GAP_SMALL))
        .h(px(brume::CONTROL_HEIGHT))
        .py(px(6.0))
        .px(px(brume::EDGE))
        .rounded(px(brume::RADIUS_CONTROL))
        .border_1()
        .border_color(brume::DIVIDER)
        .bg(if enabled {
            brume::CONTROL
        } else {
            brume::DISABLED_FILL
        })
        .text_size(px(brume::FONT_SIZE))
        .text_color(tint)
        .role(Role::Button)
        .aria_label(accessible_name)
        .tab_stop(enabled)
        .focus_visible(|style| style.border_2().border_color(brume::ACCENT));

    if let Some(icon) = icon {
        element = element.child(glyph(icon, brume::ICON_SIZE, tint));
    }

    if enabled {
        element = enabled_button(element).child(hit_layer(regions));
        if let Some(activate) = activate {
            element = with_activation(element, activate);
        }
    }
    element
}

/// One bundled SVG glyph painted at its stable asset path.
///
/// The pinned GPUI `Svg` element hashes and copies supplied bytes on every
/// construction and resolves paths from the filesystem, so it is avoided here:
/// `Window::paint_svg` receives the embedded bytes with the static path that
/// keys the sprite atlas, which parses/rasterizes an icon once per path+size.
fn glyph(icon: brume::Icon, size: f32, tint: Rgba) -> impl IntoElement {
    let path = SharedString::new_static(icon.path);
    let color: Hsla = tint.into();
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
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

/// Brume hover and press feedback for an enabled control.
fn enabled_button(element: Stateful<Div>) -> Stateful<Div> {
    element
        .hover(|style| style.bg(brume::CONTROL_HOVER))
        .active(|style| style.bg(brume::CONTROL_PRESSED))
}

/// A transparent, full-bleed prepaint listener covering the control's padded
/// box (plus the 1 px border the absolute containing block excludes).
fn hit_layer(regions: &HitRegions) -> impl IntoElement {
    let regions = regions.clone();
    canvas(move |bounds, _, _| regions.record(bounds), |_, _, _, _| {})
        .absolute()
        .inset(px(-1.0))
}

/// Wire pointer/keyboard click and the AT-SPI `Click` action to one dispatch.
fn with_activation(element: Stateful<Div>, activate: Activate) -> Stateful<Div> {
    let pointer = activate.clone();
    let accessibility = activate;
    element
        .cursor_pointer()
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
    use super::{HitRegions, OPTIONS_ID, PAUSE_RESUME_ID};
    use gpui::{Bounds, point, px, size};

    #[test]
    fn ids_are_stable() {
        assert_eq!(PAUSE_RESUME_ID, "gate-pause-resume");
        assert_eq!(OPTIONS_ID, "gate-options");
    }

    #[test]
    fn hit_regions_deduplicate_identical_rectangles() {
        let regions = HitRegions::new();
        let bounds = Bounds {
            origin: point(px(1.0), px(2.0)),
            size: size(px(60.0), px(36.0)),
        };
        regions.record(bounds);
        regions.record(bounds);
        assert_eq!(regions.rects(), vec![bounds]);
    }
}
