//! Lower-left capture session context and stream statistics for the GPUI
//! frontend.
//!
//! The block carries the operational truth (intent error, context diagnostic,
//! diagnostics fault, settings refusal, output reselection, recovery
//! evidence/stage/candidates) and, below it, the structured stream statistics
//! label. Both anchor to the bottom-left window margin; the bottom right is
//! left to the Options trigger.
//!
//! Independence rules:
//!
//! - The persisted statistics preference hides only the statistics label, not
//!   the context block: this block never reads the statistics preference.
//! - A logging/diagnostics fault is its own error line, not a capture error,
//!   and does not depend on the statistics preference.
//! - While a chrome popup is open, or a mandatory decision (reset/save/close
//!   confirmation) owns the bottom surface, the whole block is hidden.
//!
//! The recovery rows are the only focusable elements here. Each one binds the
//! rendered candidate key and the rendered generation; an activation re-checks
//! that the projection still shows that exact candidate before dispatching the
//! already-decoded `watch:epoch:candidate` tuple with the rendered attempt, so
//! a stale row can never act on newer recovery state. The key is decoded once
//! at paint time with exactly three unsigned decimal components and no
//! trailing component; a malformed key paints a disabled row and dispatches
//! nothing.
//!
//! Host integration surface:
//!
//! - `CaptureContext::new()` owns only the per-candidate focus handles.
//! - [`targets`] contributes the enabled recovery buttons to the companion's
//!   Tab chain; the companion appends them between the top ribbon and the
//!   bottom tool.
//! - [`paint`] renders the block; the companion calls it inside the root view
//!   only while chrome is painted, after the top chrome and volume rail.
//! - `stats_hidden` is the host's published preference fact (`UiUpdate`), not
//!   something this module polls or persists.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use gpui::{
    AccessibleAction, AnyElement, App, ClickEvent, Context, Div, FocusHandle, Role, SharedString,
    Stateful, TransformationMatrix, WeakEntity, Window, canvas, div, prelude::*, px,
};

use crate::ui::projection::Snapshot;

use super::{
    HostState, brume,
    companion_view::CompanionView,
    controls::{Activate, Activation, HitRegions},
    top_chrome::FocusTarget,
};

/// Width reserved on the right for the bottom Options trigger: one 48 px hit
/// target plus the shared gap.
const RIGHT_RESERVE: f32 = brume::HIT_TARGET + brume::GAP;
/// Recovery button label and accessible name.
const RECONNECT_LABEL: &str = "Reconnect";
const RECONNECT_ACCESSIBLE: &str = "Reconnect this source";
const SETTINGS_REFUSED: &str = "Settings file refused: changes remain valid for this session.";
const OUTPUT_ACTION: &str =
    "Manual audio output not found: reselect an output in the menu (capture does not restart).";

/// A decoded recovery candidate identity (`RecoveryRow.key`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecoveryToken {
    watch: u64,
    epoch: u64,
    candidate: u64,
}

/// One rendered recovery row bound to the frame that painted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RecoveryIntent {
    /// The opaque candidate key as rendered (identity guard).
    rendered_key: String,
    /// The state generation token as rendered (`Snapshot::generation`).
    rendered_generation: String,
    /// The rendered attempt, parsed from `rendered_generation`.
    attempt: u64,
    token: RecoveryToken,
}

/// Exactly three non-empty unsigned decimal components, nothing else.
///
/// The producer formats the key as `watch:epoch:candidate`; a description
/// suffix, an empty component, a sign, whitespace, a non-ASCII digit or an
/// overflowing component makes the whole key invalid.
fn decode_recovery_key(key: &str) -> Option<RecoveryToken> {
    let mut parts = key.split(':');
    let watch = unsigned_decimal(parts.next()?)?;
    let epoch = unsigned_decimal(parts.next()?)?;
    let candidate = unsigned_decimal(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(RecoveryToken {
        watch,
        epoch,
        candidate,
    })
}

/// Strict unsigned decimal token: no sign, whitespace or non-ASCII byte.
fn unsigned_decimal(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Frame-scoped state owned by `CompanionView`: only the recovery focus
/// handles, keyed by the rendered candidate key so a repaint keeps the same
/// node and a removed candidate drops it.
#[derive(Default)]
pub(super) struct CaptureContext {
    handles: RefCell<HashMap<String, FocusHandle>>,
}

impl CaptureContext {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Get-or-create the focus handle of one rendered recovery row.
    fn handle(&self, key: &str, cx: &App) -> FocusHandle {
        let mut handles = self.handles.borrow_mut();
        handles
            .entry(key.to_owned())
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    /// Drop focus handles that are not part of the rendered candidate list.
    fn retain(&self, keys: &[String]) {
        self.handles
            .borrow_mut()
            .retain(|key, _| keys.contains(key));
    }
}

/// The recovery buttons the companion appends to its Tab chain. Mirrors the
/// paint conditions: no popup, no mandatory decision, chrome visible and the
/// candidate eligible with a decoder-valid key.
pub(super) fn targets(context: &CaptureContext, state: &HostState, cx: &App) -> Vec<FocusTarget> {
    let snapshot = state.view.snapshot.as_deref();
    if suppressed(state, snapshot) {
        return Vec::new();
    }
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    if unsigned_decimal(&snapshot.generation).is_none() {
        return Vec::new();
    }
    snapshot
        .recovery
        .candidates
        .iter()
        .filter(|row| row.eligible && decode_recovery_key(&row.key).is_some())
        .map(|row| FocusTarget::plain(&context.handle(&row.key, cx)))
        .collect()
}

/// Render the bottom-left context and statistics block.
///
/// Returns an empty element when the block must not paint. The caller decides
/// whether chrome is painted at all (opacity/blank), exactly like the top
/// chrome; this function checks the popup and mandatory-decision gates itself
/// because they belong to this surface.
pub(super) fn paint(
    context: &CaptureContext,
    host: &Rc<RefCell<HostState>>,
    stats_hidden: bool,
    regions: &HitRegions,
    window: &Window,
    cx: &mut Context<CompanionView>,
) -> Div {
    let state = host.borrow();
    let snapshot = state.view.snapshot.as_deref();
    if suppressed(&state, snapshot) {
        return div();
    }
    let diagnostic = state.view.diagnostic.as_str();

    let keys: Vec<String> = snapshot
        .map(|snapshot| {
            snapshot
                .recovery
                .candidates
                .iter()
                .map(|row| row.key.clone())
                .collect()
        })
        .unwrap_or_default();
    context.retain(&keys);

    let mut children: Vec<AnyElement> = Vec::new();
    if let Some(snapshot) = snapshot {
        if let Some(error) = &snapshot.intent_error {
            let text = if error.message.is_empty() {
                error.code.to_owned()
            } else {
                error.message.clone()
            };
            children.push(label(text, Tone::Error, true).w_full().into_any_element());
        }
        if snapshot.diagnostics.has_problem {
            children.push(
                label(snapshot.diagnostics.message, Tone::Error, true)
                    .w_full()
                    .into_any_element(),
            );
        }
    }
    if !diagnostic.is_empty() {
        children.push(
            label(diagnostic, Tone::Note, true)
                .w_full()
                .into_any_element(),
        );
    }
    if let Some(snapshot) = snapshot {
        if snapshot.settings.refused {
            children.push(
                label(SETTINGS_REFUSED, Tone::Error, true)
                    .w_full()
                    .into_any_element(),
            );
        }
        if snapshot.output.needs_action {
            children.push(
                label(OUTPUT_ACTION, Tone::Error, true)
                    .w_full()
                    .into_any_element(),
            );
        }
        if !snapshot.recovery.stage.is_empty() {
            let mut text = snapshot.recovery.stage.clone();
            if !snapshot.recovery.evidence.is_empty() {
                text.push_str(" — ");
                text.push_str(&snapshot.recovery.evidence);
            }
            children.push(
                label(text, Tone::Secondary, true)
                    .w_full()
                    .into_any_element(),
            );
        }
        // The rendered attempt is the generation token of this same snapshot.
        let attempt = unsigned_decimal(&snapshot.generation);
        for row in &snapshot.recovery.candidates {
            let decoded = decode_recovery_key(&row.key);
            let action = match (row.eligible, decoded, attempt) {
                (true, Some(token), Some(attempt)) => Some(RecoveryIntent {
                    rendered_key: row.key.clone(),
                    rendered_generation: snapshot.generation.clone(),
                    attempt,
                    token,
                }),
                _ => None,
            };
            let focus = action.as_ref().map(|_| context.handle(&row.key, cx));
            children.push(
                recovery_row(
                    row.label.clone(),
                    row.detail.clone(),
                    row.key.clone(),
                    focus,
                    action,
                    host,
                    regions,
                    cx,
                )
                .into_any_element(),
            );
        }
    }

    if !stats_hidden
        && let Some(stats) = snapshot.and_then(|snapshot| snapshot.stream_stats.as_ref())
    {
        children.push(
            label(stats_text(stats), Tone::Note, true)
                .w_full()
                .into_any_element(),
        );
    }

    if children.is_empty() {
        return div();
    }

    let margin = brume::WINDOW_MARGIN;
    let width = (f32::from(window.viewport_size().width) - 2. * margin - RIGHT_RESERVE).max(1.);
    div()
        .absolute()
        .left(px(margin))
        .bottom(px(margin))
        .w(px(width))
        .flex()
        .flex_col()
        .items_start()
        .gap(px(brume::GAP_SMALL))
        .children(children)
}

/// Hidden while a popup or a mandatory decision owns the bottom surface, or
/// while the host itself refuses input.
fn suppressed(state: &HostState, snapshot: Option<&Snapshot>) -> bool {
    !accepts_chrome(state)
        || state.chrome.popup().is_some()
        || snapshot.is_some_and(|snapshot| snapshot.settings.reset_token != "0")
}

fn accepts_chrome(state: &HostState) -> bool {
    state.accepts_input() && state.view.close_dialog.is_empty() && state.overlay_opacity > 0.
}

/// One candidate row: untrusted label/detail as one plain-text line, then the
/// Reconnect button. Labels never carry input; only an enabled button records
/// a hit region.
#[allow(clippy::too_many_arguments)]
fn recovery_row(
    caption: String,
    detail: String,
    key: String,
    focus: Option<FocusHandle>,
    intent: Option<RecoveryIntent>,
    host: &Rc<RefCell<HostState>>,
    regions: &HitRegions,
    cx: &mut Context<CompanionView>,
) -> Div {
    let mut text = caption;
    if !detail.is_empty() {
        text.push_str(" — ");
        text.push_str(&detail);
    }
    let button = match (focus, intent) {
        (Some(focus), Some(intent)) => {
            let activate = recovery_activation(intent, Rc::clone(host), cx);
            reconnect_button(key, Some(&focus), true, Some(activate), regions).into_any_element()
        }
        _ => reconnect_button(key, None, false, None, regions).into_any_element(),
    };
    div()
        .w_full()
        .flex()
        .items_start()
        .gap(px(brume::GAP_SMALL))
        .child(label(text, Tone::Note, true).flex_1().min_w(px(0.)))
        .child(button)
}

fn recovery_activation(
    intent: RecoveryIntent,
    host: Rc<RefCell<HostState>>,
    cx: &Context<CompanionView>,
) -> Activate {
    let view = cx.weak_entity();
    Rc::new(move |source, _window, cx| {
        {
            let mut state = host.borrow_mut();
            if !state.dispatch_recovery(&intent) {
                return;
            }
            state.focus_intent |= source == Activation::Accessibility;
        }
        notify(view.clone(), cx);
    })
}

fn notify(view: WeakEntity<CompanionView>, cx: &mut App) {
    let _ = view.update(cx, |_, cx| cx.notify());
}

/// Brume text button with a dynamic element id. `controls::button` requires a
/// `'static` identity, but recovery rows are keyed by their opaque candidate
/// key, so the shared button styling is reproduced here for a `SharedString`
/// identity (same 36 px box, border, glyph and focus ring).
fn reconnect_button(
    key: String,
    focus: Option<&FocusHandle>,
    enabled: bool,
    activate: Option<Activate>,
    regions: &HitRegions,
) -> impl IntoElement {
    let id = SharedString::from(format!("context-recovery-{key}"));
    let tint = if enabled {
        brume::FOREGROUND
    } else {
        brume::DISABLED_TEXT
    };
    let mut element = div()
        .id(id.clone())
        .accessibility_id(id)
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
        .aria_label(RECONNECT_ACCESSIBLE)
        .tab_stop(enabled);
    if let Some(focus) = focus {
        element = element
            .track_focus(focus)
            .focus_visible(|style| style.border_2().border_color(brume::ACCENT));
    }
    if let Some(icon) = brume::icon("refresh") {
        element = element.child(glyph(icon, brume::ICON_SIZE, tint));
    }
    element = element.child(RECONNECT_LABEL);
    if enabled {
        element = element
            .hover(|style| style.bg(brume::CONTROL_HOVER))
            .active(|style| style.bg(brume::CONTROL_PRESSED))
            .child(hit_layer(regions));
        if let Some(activate) = activate {
            element = with_activation(element, activate);
        }
    }
    element
}

fn with_activation(element: Stateful<Div>, activate: Activate) -> Stateful<Div> {
    let pointer = activate.clone();
    let accessibility = activate;
    element
        .cursor_pointer()
        .on_click(
            move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                let source = match event {
                    ClickEvent::Keyboard(_) => Activation::Keyboard,
                    ClickEvent::Mouse(_) | ClickEvent::Touch(_) => Activation::Pointer,
                };
                pointer(source, window, cx);
            },
        )
        .on_a11y_action(
            AccessibleAction::Click,
            move |_: Option<&gpui::accesskit::ActionData>, window, cx| {
                accessibility(Activation::Accessibility, window, cx);
            },
        )
}

fn hit_layer(regions: &HitRegions) -> impl IntoElement {
    let regions = regions.clone();
    canvas(move |bounds, _, _| regions.record(bounds), |_, _, _, _| {})
        .absolute()
        .inset(px(-1.0))
}

/// One bundled SVG glyph painted at its stable asset path, like every other
/// Brume control (the pinned GPUI `Svg` element copies bytes per frame).
fn glyph(icon: brume::Icon, size: f32, tint: gpui::Rgba) -> impl IntoElement {
    let path = SharedString::new_static(icon.path);
    let color: gpui::Hsla = tint.into();
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    Secondary,
    Note,
    Error,
}

/// Plain-text Brume label in the secondary, note or error variant. GPUI text is never
/// interpreted as markup, so device names, diagnostics and recovery text paint
/// directly.
fn label(text: impl Into<SharedString>, tone: Tone, enabled: bool) -> Div {
    let text: SharedString = text.into();
    let (size, color) = match tone {
        Tone::Secondary => (brume::FONT_SIZE, brume::SECONDARY),
        Tone::Note => (brume::FONT_SIZE_SMALL, brume::SECONDARY),
        Tone::Error => (brume::FONT_SIZE, brume::ERROR),
    };
    div()
        .text_size(px(size))
        .font_weight(brume::FONT_WEIGHT_NORMAL)
        .text_color(if enabled { color } else { brume::DISABLED_TEXT })
        .child(text)
}

/// The structured statistics block. The human FPS label is the only decimal
/// conversion here; it is derived from the projected rational and never feeds a
/// dispatch.
fn stats_text(stats: &crate::ui::projection::StreamStats) -> String {
    format!(
        "{}\nInput policy: {} · {}×{} · {}\nResolved input: {}\nAudio {}: {}",
        stats.identity_text,
        stats.input_policy,
        stats.size.width,
        stats.size.height,
        super::controls::rate_fps(&stats.rate.text),
        stats.resolved_input,
        if stats.audio_enabled {
            "enabled"
        } else {
            "disabled (retained)"
        },
        if stats.audio_input_label.is_empty() {
            "none"
        } else {
            stats.audio_input_label.as_str()
        },
    )
}

impl HostState {
    /// Dispatch one rendered recovery choice through the checked runtime
    /// intent.
    ///
    /// Re-checks the rendered identity before acting: the projection must still
    /// carry the same generation and still offer the same eligible candidate.
    /// The attempt handed to `choose_recovery` is the rendered one, never the
    /// current attempt, and the runtime keeps its own stale guard.
    pub(super) fn dispatch_recovery(&mut self, intent: &RecoveryIntent) -> bool {
        if !accepts_chrome(self) || self.chrome.popup().is_some() {
            return false;
        }
        let Some(snapshot) = self.view.snapshot.as_deref() else {
            return false;
        };
        if snapshot.generation != intent.rendered_generation
            || !snapshot
                .recovery
                .candidates
                .iter()
                .any(|row| row.key == intent.rendered_key && row.eligible)
        {
            return false;
        }
        let update = self.runtime.choose_recovery(
            intent.attempt,
            intent.token.watch,
            intent.token.epoch,
            intent.token.candidate,
        );
        self.consume(update);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_host_capture_context_decodes_exact_three_component_keys() {
        assert_eq!(
            decode_recovery_key("7:42:3"),
            Some(RecoveryToken {
                watch: 7,
                epoch: 42,
                candidate: 3,
            })
        );
        // Zero is a valid unsigned component here; the runtime refuses zero
        // identifiers with its own guard.
        assert_eq!(
            decode_recovery_key("0:0:0"),
            Some(RecoveryToken {
                watch: 0,
                epoch: 0,
                candidate: 0,
            })
        );
        assert_eq!(
            decode_recovery_key("18446744073709551615:1:2"),
            Some(RecoveryToken {
                watch: u64::MAX,
                epoch: 1,
                candidate: 2,
            })
        );
    }

    #[test]
    fn gpui_host_capture_context_refuses_malformed_recovery_keys() {
        for key in [
            "",
            "1:2",
            "1:2:3:4",
            "1:2:3|USB",
            "1:2:3 ",
            " 1:2:3",
            "1::3",
            ":1:3",
            "1:2:",
            "+1:2:3",
            "1:-2:3",
            "1:2:3x",
            "１:2:3",
            "18446744073709551616:1:1",
        ] {
            assert_eq!(decode_recovery_key(key), None, "{key:?}");
        }
    }
}
