//! Rendering settings body. The selection is requested state; capability and
//! admission remain the runtime's responsibility.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use gpui::{
    App, Context, Div, Entity, FocusHandle, Role, SharedString, Stateful, Subscription, Window,
    accesskit::Toggled, div, prelude::*, px,
};

use crate::{
    domain::render::{RendererValues, ScalerChoice},
    ui::projection::{RenderFieldRow, Snapshot},
};

use super::{
    HostState, brume,
    chrome::Popup,
    companion_view::CompanionView,
    controls::{Activate, Activation, HitRegions, button},
    text_field::TextField,
};

const FIELDS: [&str; 9] = [
    "scale",
    "cscale",
    "dscale",
    "dither",
    "scale_antiring",
    "deband",
    "deband_iterations",
    "deband_grain",
    "interpolation",
];

#[derive(Default)]
pub(super) struct RendererPage {
    active: Rc<Cell<Option<Popup>>>,
    choice: Rc<Cell<Option<&'static str>>>,
    source: String,
    revision: Rc<str>,
    fields: HashMap<&'static str, FocusHandle>,
    options: HashMap<(&'static str, String), FocusHandle>,
    editors: HashMap<&'static str, NumberEditor>,
    focus: Vec<FocusHandle>,
}

impl RendererPage {
    /// Call on page departure, after finishing any focused edit while this
    /// popup is still valid. Invalidate old callbacks even if Back and
    /// Rendering reuse the containing menu's popup epoch.
    pub(super) fn clear(&mut self) {
        self.active.set(None);
        *self = Self::default();
    }

    pub(super) fn focus_handles(&self) -> &[FocusHandle] {
        &self.focus
    }

    pub(super) fn active_edit(&self, window: &Window) -> Option<&FocusHandle> {
        self.editors.values().find_map(|editor| {
            (editor.projection.borrow().enabled && editor.focus.is_focused(window))
                .then_some(&editor.focus)
        })
    }

    pub(super) fn finish_edit(&self, expected: &FocusHandle, state: &mut HostState, cx: &mut App) {
        if let Some(editor) = self
            .editors
            .values()
            .find(|editor| editor.focus == *expected)
        {
            finish_number(&editor.projection.borrow(), &editor.field, state, cx);
        }
    }

    pub(super) fn paint(
        &mut self,
        popup: Popup,
        snapshot: &Snapshot,
        state: &Rc<RefCell<HostState>>,
        regions: &HitRegions,
        window: &mut Window,
        cx: &mut Context<CompanionView>,
    ) -> Div {
        let source = snapshot
            .selection
            .as_ref()
            .map_or("", |s| s.source_key.as_str());
        if self.active.get() != Some(popup) || self.source != source {
            self.clear();
            self.active.set(Some(popup));
            self.source.push_str(source);
        }
        self.focus.clear();
        let renderer = &snapshot.renderer;
        let enabled = editable(snapshot);
        if !enabled {
            self.choice.set(None);
        }
        let mut body = div().w_full().flex().flex_col().gap(px(brume::GAP));
        if !renderer.error.is_empty() {
            body = body.child(note(&renderer.error, true));
        }
        if !renderer.notice.is_empty() {
            body = body.child(note(&renderer.notice, false));
        }
        if renderer.fields.is_empty() {
            body = body.child(note(
                if renderer.available {
                    "No rendering field reported."
                } else {
                    "Rendering catalog unavailable."
                },
                false,
            ));
        }
        let Some(selection) = &snapshot.selection else {
            return body;
        };
        let values = selection.renderer.values();
        if self.revision.as_ref() != snapshot.selection_revision {
            self.revision = Rc::from(snapshot.selection_revision.as_str());
        }
        for key in FIELDS {
            let Some(row) = renderer.fields.iter().find(|row| row.key == key) else {
                self.editors.remove(key);
                continue;
            };
            let guard = RenderGuard {
                active: self.active.clone(),
                popup,
                revision: self.revision.clone(),
                key,
            };
            let mut field = div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(brume::GAP_SMALL))
                .child(div().text_color(brume::SECONDARY).child(row.label.clone()));
            match (row.field_type, key) {
                ("choice", "scale" | "cscale" | "dscale" | "dither") => {
                    let selected = match key {
                        "scale" => values.scale.as_str(),
                        "cscale" => scaler(values.cscale),
                        "dscale" => scaler(values.dscale),
                        _ => values.dither.as_str(),
                    };
                    field = field.child(
                        self.paint_choice(row, selected, enabled, guard, state, regions, cx),
                    );
                }
                ("boolean", "deband" | "interpolation") => {
                    let checked = if key == "deband" {
                        values.deband.enabled
                    } else {
                        values.interpolation
                    };
                    let handle = self.fields.entry(key).or_insert_with(|| cx.focus_handle());
                    let action = enabled.then(|| {
                        activation(state, cx, guard, move |state, guard| {
                            let update = if key == "deband" {
                                state.runtime.set_deband_enabled(&guard.revision, !checked)
                            } else {
                                state.runtime.set_interpolation(&guard.revision, !checked)
                            };
                            state.consume(update);
                            None
                        })
                    });
                    if enabled {
                        self.focus.push(handle.clone());
                    }
                    field = field.child(
                        button(
                            key,
                            if checked { "Enabled" } else { "Disabled" },
                            None,
                            enabled,
                            regions,
                            action,
                        )
                        .role(Role::CheckBox)
                        .aria_label(row.label.clone())
                        .aria_toggled(toggled(checked))
                        .when(enabled, |el| el.track_focus(handle)),
                    );
                }
                ("number", "scale_antiring" | "deband_grain")
                | ("integer", "deband_iterations") => {
                    let bounds = Bounds {
                        min: row.min.unwrap_or(0.),
                        max: row.max.unwrap_or(0.),
                        integer: row.field_type == "integer",
                    };
                    let projected = numeric_value(values, key);
                    let editor = self.editors.entry(key).or_insert_with(|| {
                        NumberEditor::new(row, projected, bounds, guard.clone(), state, window, cx)
                    });
                    editor.sync(projected, bounds, guard, enabled, window, cx);
                    if enabled {
                        self.focus.push(editor.focus.clone());
                    }
                    let valid = parse_number(editor.field.read(cx).text(), bounds).is_some();
                    field = field.child(
                        div()
                            .w_full()
                            .rounded(px(brume::RADIUS_CONTROL))
                            .when(!valid, |el| el.border_1().border_color(brume::ERROR))
                            .child(editor.field.clone()),
                    );
                    let step = row
                        .step
                        .map_or_else(String::new, |step| format!(" · Step {step}"));
                    field =
                        field.child(note(&format!("{}–{}{step}", bounds.min, bounds.max), false));
                }
                _ => continue,
            }
            if !row.notice.is_empty() {
                field = field.child(note(&row.notice, false));
            }
            body = body.child(field);
        }
        body
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_choice(
        &mut self,
        row: &RenderFieldRow,
        selected: &'static str,
        enabled: bool,
        guard: RenderGuard,
        state: &Rc<RefCell<HostState>>,
        regions: &HitRegions,
        cx: &Context<CompanionView>,
    ) -> Stateful<Div> {
        let key = row.key;
        let enabled = enabled && row.choices.len() > 1;
        let open = enabled && self.choice.get() == Some(key);
        let handle = self
            .fields
            .entry(key)
            .or_insert_with(|| cx.focus_handle())
            .clone();
        let label = row
            .choices
            .iter()
            .find(|option| option.key == selected)
            .map_or_else(
                || SharedString::new_static(selected),
                |option| option.label.clone().into(),
            );
        let activate = enabled.then(|| {
            let choice = self.choice.clone();
            let target = handle.clone();
            activation(state, cx, guard.clone(), move |_, _| {
                choice.set((choice.get() != Some(key)).then_some(key));
                Some(target.clone())
            })
        });
        if enabled {
            self.focus.push(handle.clone());
        }
        let mut list = div()
            .id(format!("renderer-{key}-choice"))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(brume::GAP_SMALL))
            .child(
                button(
                    key,
                    label.clone(),
                    brume::icon("chevron-down"),
                    enabled,
                    regions,
                    activate,
                )
                .role(Role::ComboBox)
                .aria_label(row.label.clone())
                .aria_expanded(open)
                .aria_value(label)
                .when(enabled, |el| el.track_focus(&handle)),
            );
        if !open {
            return list;
        }
        for option in &row.choices {
            let id: SharedString = format!("renderer-{key}-{}", option.key).into();
            let focus = self
                .options
                .entry((key, option.key.clone()))
                .or_insert_with(|| cx.focus_handle());
            let active = option.key == selected;
            let activate = option.eligible.then(|| {
                let token = option.key.clone();
                let choice = self.choice.clone();
                let restore = handle.clone();
                activation(state, cx, guard.clone(), move |state, guard| {
                    if choice.get() != Some(key) {
                        return None;
                    }
                    choice.set(None);
                    let update = match key {
                        "scale" => state.runtime.set_scale(&guard.revision, &token),
                        "cscale" => state.runtime.set_cscale(&guard.revision, &token),
                        "dscale" => state.runtime.set_dscale(&guard.revision, &token),
                        _ => state.runtime.set_dither(&guard.revision, &token),
                    };
                    state.consume(update);
                    Some(restore.clone())
                })
            });
            if option.eligible {
                self.focus.push(focus.clone());
            }
            list = list.child(
                div().id(id.clone()).w_full().child(
                    button(
                        "renderer-choice-option",
                        option.label.clone(),
                        None,
                        option.eligible,
                        regions,
                        activate,
                    )
                    .accessibility_id(id)
                    .role(Role::RadioButton)
                    .aria_description(option.reason.clone())
                    .aria_toggled(toggled(active))
                    .aria_selected(active)
                    .when(active && option.eligible, |el| {
                        el.border_color(brume::ACCENT)
                    })
                    .when(option.eligible, |el| el.track_focus(focus)),
                ),
            );
        }
        list
    }
}

fn scaler(choice: ScalerChoice) -> &'static str {
    match choice {
        ScalerChoice::Inherit => "auto",
        ScalerChoice::Kernel(kernel) => kernel.as_str(),
    }
}

fn numeric_value(values: RendererValues, key: &str) -> f64 {
    match key {
        "scale_antiring" => values.scale_antiring,
        "deband_iterations" => f64::from(values.deband.iterations),
        _ => values.deband.grain,
    }
}

fn toggled(value: bool) -> Toggled {
    if value { Toggled::True } else { Toggled::False }
}

fn note(text: &str, error: bool) -> Div {
    div()
        .w_full()
        .text_size(px(brume::FONT_SIZE_SMALL))
        .text_color(if error {
            brume::ERROR
        } else {
            brume::SECONDARY
        })
        .child(text.to_owned())
}

fn editable(snapshot: &Snapshot) -> bool {
    snapshot.renderer.available
        && !snapshot.closing
        && snapshot.can_edit_selection
        && snapshot.selection.is_some()
        && snapshot.settings.reset_token == "0"
}

#[derive(Clone)]
struct RenderGuard {
    active: Rc<Cell<Option<Popup>>>,
    popup: Popup,
    revision: Rc<str>,
    key: &'static str,
}

impl RenderGuard {
    fn allows(&self, state: &HostState) -> bool {
        self.active.get() == Some(self.popup)
            && state.chrome.popup() == Some(self.popup)
            && state.accepts_input()
            && state.overlay_opacity > 0.
            && state.view.close_dialog.is_empty()
            && state.view.snapshot.as_ref().is_some_and(|s| editable(s))
            && self.revision.bytes().all(|byte| byte.is_ascii_digit())
            && (self.revision.len() == 1 || !self.revision.starts_with('0'))
            && self.revision.parse::<u64>().is_ok()
    }
}

fn activation(
    state: &Rc<RefCell<HostState>>,
    cx: &Context<CompanionView>,
    guard: RenderGuard,
    run: impl Fn(&mut HostState, &RenderGuard) -> Option<FocusHandle> + 'static,
) -> Activate {
    let state = state.clone();
    let view = cx.weak_entity();
    Rc::new(move |source, window, cx| {
        let focus = {
            let mut state = state.borrow_mut();
            if !guard.allows(&state) {
                return;
            }
            state.focus_intent |= source == Activation::Accessibility;
            run(&mut state, &guard)
        };
        if let Some(focus) = focus {
            focus.focus(window, cx);
        }
        let _ = view.update(cx, |_, cx| cx.notify());
    })
}

#[derive(Clone, Copy)]
struct Bounds {
    min: f64,
    max: f64,
    integer: bool,
}

enum Number {
    Integer(i32),
    Decimal(f64),
}

fn parse_number(text: &str, bounds: Bounds) -> Option<Number> {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let mut digits = false;
    let mut dot = false;
    for byte in unsigned.bytes() {
        match byte {
            b'0'..=b'9' => digits = true,
            b'.' if !bounds.integer && !dot => dot = true,
            _ => return None,
        }
    }
    if !digits {
        return None;
    }
    let value: f64 = text.parse().ok()?;
    if !value.is_finite() || !(bounds.min..=bounds.max).contains(&value) {
        return None;
    }
    if bounds.integer {
        text.parse().ok().map(Number::Integer)
    } else {
        Some(Number::Decimal(value))
    }
}

struct NumericProjection {
    guard: RenderGuard,
    bounds: Bounds,
    enabled: bool,
    value: f64,
}

struct NumberEditor {
    field: Entity<TextField>,
    focus: FocusHandle,
    projection: Rc<RefCell<NumericProjection>>,
    _blur: Subscription,
    _observe: Subscription,
}

impl NumberEditor {
    fn new(
        row: &RenderFieldRow,
        value: f64,
        bounds: Bounds,
        guard: RenderGuard,
        state: &Rc<RefCell<HostState>>,
        window: &mut Window,
        cx: &mut Context<CompanionView>,
    ) -> Self {
        let field = cx.new(|cx| TextField::new(row.label.clone(), value.to_string(), cx));
        let focus = field.read(cx).handle();
        let projection = Rc::new(RefCell::new(NumericProjection {
            guard,
            bounds,
            enabled: true,
            value,
        }));
        let blur = {
            let state = state.clone();
            let projection = projection.clone();
            let field = field.clone();
            cx.on_focus_out(&focus, window, move |_, _, _, cx| {
                finish_number(&projection.borrow(), &field, &mut state.borrow_mut(), cx);
            })
        };
        let observe = cx.observe(&field, |_, _, cx| cx.notify());
        Self {
            field,
            focus,
            projection,
            _blur: blur,
            _observe: observe,
        }
    }

    fn sync(
        &self,
        value: f64,
        bounds: Bounds,
        guard: RenderGuard,
        enabled: bool,
        window: &Window,
        cx: &mut App,
    ) {
        let mut projection = self.projection.borrow_mut();
        if projection.value != value
            && !self.focus.is_focused(window)
            && parse_number(self.field.read(cx).text(), bounds).is_some()
        {
            self.field
                .update(cx, |field, cx| field.reset(value.to_string(), cx));
        }
        if projection.enabled != enabled {
            self.field
                .update(cx, |field, cx| field.set_enabled(enabled, cx));
        }
        *projection = NumericProjection {
            guard,
            bounds,
            enabled,
            value,
        };
    }
}

fn finish_number(
    editor: &NumericProjection,
    field: &Entity<TextField>,
    state: &mut HostState,
    cx: &mut App,
) {
    if !editor.enabled || !editor.guard.allows(state) {
        return;
    }
    let input = field.read(cx);
    if !input.is_dirty() {
        return;
    }
    let Some(value) = parse_number(input.text(), editor.bounds) else {
        return;
    };
    // A valid completion consumes the pending edit before dispatch, including
    // a refusal. Enter and the following blur cannot replay it.
    field.update(cx, |field, _| field.take_edit());
    let revision = &editor.guard.revision;
    let update = match (editor.guard.key, value) {
        ("scale_antiring", Number::Decimal(value)) => {
            state.runtime.set_scale_antiring(revision, value)
        }
        ("deband_iterations", Number::Integer(value)) => {
            state.runtime.set_deband_iterations(revision, value)
        }
        ("deband_grain", Number::Decimal(value)) => state.runtime.set_deband_grain(revision, value),
        _ => return,
    };
    state.consume(update);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_host_renderer_numeric_field_accepts_standard_decimals_without_quantizing() {
        let bounds = Bounds {
            min: 0.,
            max: 1.,
            integer: false,
        };
        assert!(matches!(parse_number("+.031", bounds), Some(Number::Decimal(n)) if n == 0.031));
        assert!(matches!(
            parse_number("1.", bounds),
            Some(Number::Decimal(1.))
        ));
        for text in [
            "", ".", "0.5e0", " 0.5", "NaN", "0,5", "0.5.0", "-0.01", "1.01",
        ] {
            assert!(parse_number(text, bounds).is_none(), "{text}");
        }
    }

    #[test]
    fn gpui_host_renderer_iterations_accepts_only_bounded_signed_integer_syntax() {
        let bounds = Bounds {
            min: 0.,
            max: 16.,
            integer: true,
        };
        assert!(matches!(
            parse_number("-0", bounds),
            Some(Number::Integer(0))
        ));
        assert!(matches!(
            parse_number("+16", bounds),
            Some(Number::Integer(16))
        ));
        for text in ["+", "2.0", "1e1", "-1", "17", "4294967296"] {
            assert!(parse_number(text, bounds).is_none(), "{text}");
        }
    }
}
