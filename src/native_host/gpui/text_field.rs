//! Single-line editor adapted from GPUI 0.3.13's `examples/input.rs` (Apache-2.0).
//! Completion and validation belong to the panel, not the input handler.

use std::{borrow::Cow, ops::Range};

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, GlobalElementId, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ShapedLine, SharedString, Style,
    TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px,
    relative,
};

use super::brume;

actions!(
    furami_text_field,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        Home,
        End,
        SelectHome,
        SelectEnd,
        SelectAll,
        Cut,
        Copy,
        Paste
    ]
);

pub(super) fn register_keys(cx: &mut App) {
    let scope = Some("FuramiTextField");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, scope),
        KeyBinding::new("delete", Delete, scope),
        KeyBinding::new("left", Left, scope),
        KeyBinding::new("right", Right, scope),
        KeyBinding::new("shift-left", SelectLeft, scope),
        KeyBinding::new("shift-right", SelectRight, scope),
        KeyBinding::new("home", Home, scope),
        KeyBinding::new("end", End, scope),
        KeyBinding::new("shift-home", SelectHome, scope),
        KeyBinding::new("shift-end", SelectEnd, scope),
        KeyBinding::new("ctrl-a", SelectAll, scope),
        KeyBinding::new("ctrl-x", Cut, scope),
        KeyBinding::new("ctrl-c", Copy, scope),
        KeyBinding::new("ctrl-v", Paste, scope),
    ]);
}

pub(super) struct TextField {
    label: SharedString,
    text: SharedString,
    focus: FocusHandle,
    // Byte offsets, always on scalar boundaries. IME offsets are UTF-16.
    anchor: usize,
    caret: usize,
    marked: Option<Range<usize>>,
    composition_base: Option<SharedString>,
    pending_edit: bool,
    enabled: bool,
    drag: bool,
    layout: Option<LineLayout>,
}

impl TextField {
    pub(super) fn new(
        label: impl Into<SharedString>,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = initial_line(text.into());
        Self {
            label: label.into(),
            anchor: text.len(),
            caret: text.len(),
            composition_base: None,
            pending_edit: false,
            enabled: true,
            text,
            focus: cx.focus_handle(),
            marked: None,
            drag: false,
            layout: None,
        }
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn reset(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.text = initial_line(text.into());
        self.caret = self.text.len();
        self.anchor = self.caret;
        self.marked = None;
        self.drag = false;
        self.layout = None;
        self.composition_base = None;
        self.pending_edit = false;
        cx.notify();
    }

    pub(super) fn handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub(super) fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.drag = false;
            cx.notify();
        }
    }

    pub(super) fn is_composing(&self) -> bool {
        self.marked.is_some()
    }

    /// A preedit buffer is never eligible for panel completion.
    pub(super) fn is_dirty(&self) -> bool {
        self.marked.is_none() && self.pending_edit
    }

    pub(super) fn take_edit(&mut self) -> SharedString {
        self.pending_edit = false;
        self.text.clone()
    }

    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }

    fn move_caret(&mut self, offset: usize, extend: bool, window: &Window, cx: &mut Context<Self>) {
        if !extend {
            self.anchor = offset;
        }
        self.caret = offset;
        window.notify_text_selection_changed(&self.focus);
        cx.notify();
    }

    fn previous(&self) -> usize {
        self.text[..self.caret]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.caret
            + self.text[self.caret..]
                .chars()
                .next()
                .map_or(0, char::len_utf8)
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.anchor == self.caret && self.marked.is_none() {
            self.anchor = self.previous();
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.anchor == self.caret && self.marked.is_none() {
            self.anchor = self.next();
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.selection();
        if !range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.text[range].to_owned()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        let range = self.selection();
        if !range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.text[range.clone()].to_owned(),
            ));
            self.replace_text_in_range(Some(to_utf16(&self.text, range)), "", window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn pointer_index(&self, position: Point<Pixels>) -> usize {
        self.layout
            .as_ref()
            .filter(|layout| layout.line.text == self.text)
            .map_or(self.caret, |layout| {
                layout
                    .line
                    .closest_index_for_x(position.x - layout.origin.x)
            })
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        window.request_virtual_keyboard();
        self.drag = true;
        self.move_caret(
            self.pointer_index(event.position),
            event.modifiers.shift,
            window,
            cx,
        );
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.drag {
            self.move_caret(self.pointer_index(event.position), true, window, cx);
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.drag = false;
    }

    fn replace(&mut self, range: Option<Range<usize>>, inserted: &str) -> (usize, bool) {
        let range = range
            .map(|range| from_utf16(&self.text, range))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection());
        let changed = self.text[range.clone()] != *inserted;
        if changed {
            let mut text = String::with_capacity(self.text.len() - range.len() + inserted.len());
            text.push_str(&self.text[..range.start]);
            text.push_str(inserted);
            text.push_str(&self.text[range.end..]);
            self.text = text.into();
        }
        self.anchor = range.start + inserted.len();
        self.caret = self.anchor;
        (range.start, changed)
    }
}

fn initial_line(text: SharedString) -> SharedString {
    match single_line(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(text) => text.into(),
    }
}

fn single_line(text: &str) -> Cow<'_, str> {
    const BREAKS: [char; 5] = ['\r', '\n', '\u{85}', '\u{2028}', '\u{2029}'];
    if text.contains(BREAKS) {
        // One UTF-16 unit in and out: IME selection offsets remain meaningful.
        Cow::Owned(text.replace(BREAKS, " "))
    } else {
        Cow::Borrowed(text)
    }
}

fn byte_at_utf16(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= offset {
            return byte;
        }
        units += ch.len_utf16();
    }
    text.len()
}

fn from_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    byte_at_utf16(text, range.start)..byte_at_utf16(text, range.end.max(range.start))
}

fn to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    let start = text[..range.start].encode_utf16().count();
    start..start + text[range].encode_utf16().count()
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = from_utf16(&self.text, range);
        *actual = Some(to_utf16(&self.text, range.clone()));
        Some(self.text[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: to_utf16(&self.text, self.selection()),
            reversed: self.caret < self.anchor,
        })
    }

    fn set_selected_text_range(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = from_utf16(&self.text, range);
        self.anchor = range.start;
        self.move_caret(range.end, true, window, cx);
    }

    fn text_length_utf16(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.text.encode_utf16().count())
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.clone().map(|range| to_utf16(&self.text, range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        if let Some(before) = self.composition_base.take() {
            self.pending_edit |= self.text != before;
        }
        window.notify_text_input_changed(&self.focus);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (_, changed) = self.replace(range, &single_line(text));
        if self.composition_base.is_none() {
            self.pending_edit |= changed;
        }
        self.unmark_text(window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = single_line(text);
        if self.composition_base.is_none() {
            self.composition_base = Some(self.text.clone());
        }
        let (start, _) = self.replace(range, &text);
        self.marked = (!text.is_empty()).then_some(start..self.caret);
        if let Some(selection) = selection {
            // The supplied selection is relative to the inserted preedit, not
            // to the document. Convert against that slice before translating.
            let selection = from_utf16(&text, selection);
            self.anchor = start + selection.start;
            self.caret = start + selection.end;
        }
        if self.marked.is_none() {
            self.unmark_text(window, cx);
        } else {
            window.notify_text_input_changed(&self.focus);
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        (layout.line.text == self.text).then(|| layout.range_bounds(from_utf16(&self.text, range)))
    }

    fn selection_bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Bounds<Pixels>> {
        self.bounds_for_range(range, bounds, window, cx)
            .into_iter()
            .collect()
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self
            .layout
            .as_ref()
            .filter(|layout| layout.line.text == self.text)?;
        layout.bounds.localize(&point)?;
        let byte = layout.line.closest_index_for_x(point.x - layout.origin.x);
        Some(self.text[..byte].encode_utf16().count())
    }
}

struct LineLayout {
    line: ShapedLine,
    bounds: Bounds<Pixels>,
    origin: Point<Pixels>,
}

impl LineLayout {
    fn range_bounds(&self, range: Range<usize>) -> Bounds<Pixels> {
        let start = self.origin.x + self.line.x_for_index(range.start);
        let end = self.origin.x + self.line.x_for_index(range.end);
        Bounds::from_corners(
            point(start.min(end), self.bounds.top()),
            point(start.max(end), self.bounds.bottom()),
        )
    }
}

struct TextElement(Entity<TextField>);

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<LineLayout>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let field = self.0.read(cx);
        let style = window.text_style();
        let run = TextRun {
            len: field.text.len(),
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let marked_runs;
        let runs = if let Some(marked) = field.marked.as_ref() {
            marked_runs = [
                TextRun {
                    len: marked.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked.len(),
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: run.len - marked.end,
                    ..run
                },
            ];
            let first = usize::from(marked.start == 0);
            let end = marked_runs.len() - usize::from(marked.end == field.text.len());
            &marked_runs[first..end]
        } else {
            std::slice::from_ref(&run)
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(field.text.clone(), font_size, runs, None);
        let width = (bounds.size.width - px(1.)).max(px(0.));
        let previous = field
            .layout
            .as_ref()
            .map_or(px(0.), |layout| layout.bounds.left() - layout.origin.x);
        let caret_x = line.x_for_index(field.caret);
        let offset = previous
            .clamp((caret_x - width).max(px(0.)), caret_x.max(px(0.)))
            .min((line.width - width).max(px(0.)));
        Some(LineLayout {
            line,
            bounds,
            origin: point(bounds.left() - offset, bounds.top()),
        })
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(layout) = prepaint.take() else {
            return;
        };
        let field = self.0.read(cx);
        let focus = field.handle();
        let selection = field.selection();
        let focused = field.enabled && focus.is_focused(window);
        if field.enabled {
            window.handle_input(&focus, ElementInputHandler::new(bounds, self.0.clone()), cx);
        }
        if focused && !selection.is_empty() {
            window.paint_quad(fill(
                layout.range_bounds(selection.clone()),
                brume::SURFACE2,
            ));
        }
        let _ = layout.line.paint(
            layout.origin,
            bounds.size.height,
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );
        if focused && selection.is_empty() {
            let mut caret = layout.range_bounds(selection);
            caret.size.width = px(1.);
            window.paint_quad(fill(caret, brume::ACCENT));
        }
        self.0.update(cx, |field, _| field.layout = Some(layout));
    }
}

impl Render for TextField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(ElementId::Name(self.label.clone()))
            .role(gpui::Role::TextInput)
            .aria_label(self.label.clone())
            .aria_value(self.text.clone())
            .flex()
            .items_center()
            .h(px(brume::HIT_TARGET))
            .w_full()
            .min_w(px(0.))
            .px(px(brume::GAP))
            .rounded(px(brume::RADIUS_CONTROL))
            .bg(if self.enabled {
                brume::CONTROL
            } else {
                brume::DISABLED_FILL
            })
            .border_1()
            .border_color(brume::SEPARATOR)
            .when(self.enabled, |field| {
                field
                    .track_focus(&self.focus)
                    .key_context("FuramiTextField")
                    .cursor(CursorStyle::IBeam)
                    .focus(|style| style.border_color(brume::ACCENT))
            })
            .text_color(brume::FOREGROUND)
            .text_size(px(brume::FONT_SIZE))
            .line_height(px(20.))
            .when(self.enabled, |field| {
                field
                    .on_action(cx.listener(Self::backspace))
                    .on_action(cx.listener(Self::delete))
                    .on_action(cx.listener(Self::copy))
                    .on_action(cx.listener(Self::cut))
                    .on_action(cx.listener(Self::paste))
                    .on_action(cx.listener(|this, _: &Left, window, cx| {
                        this.move_caret(
                            if this.anchor == this.caret {
                                this.previous()
                            } else {
                                this.selection().start
                            },
                            false,
                            window,
                            cx,
                        );
                    }))
                    .on_action(cx.listener(|this, _: &Right, window, cx| {
                        this.move_caret(
                            if this.anchor == this.caret {
                                this.next()
                            } else {
                                this.selection().end
                            },
                            false,
                            window,
                            cx,
                        );
                    }))
                    .on_action(cx.listener(|this, _: &SelectLeft, window, cx| {
                        this.move_caret(this.previous(), true, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectRight, window, cx| {
                        this.move_caret(this.next(), true, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &Home, window, cx| {
                        this.move_caret(0, false, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &End, window, cx| {
                        this.move_caret(this.text.len(), false, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectHome, window, cx| {
                        this.move_caret(0, true, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectEnd, window, cx| {
                        this.move_caret(this.text.len(), true, window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &SelectAll, window, cx| {
                        this.anchor = 0;
                        this.move_caret(this.text.len(), true, window, cx);
                    }))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
                    .on_mouse_move(cx.listener(Self::mouse_move))
            })
            .child(
                div()
                    .w_full()
                    .overflow_hidden()
                    .child(TextElement(cx.entity())),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_host_text_field_utf16_ranges_keep_byte_boundaries() {
        let text = "a🦊éz";
        assert_eq!(from_utf16(text, 1..3), 1..5);
        assert_eq!(from_utf16(text, 2..4), 5..7);
        assert_eq!(from_utf16(text, 3..99), 5..8);
        assert_eq!(to_utf16(text, 1..7), 1..4);
    }

    #[test]
    fn gpui_host_text_field_normalizes_line_breaks_without_shifting_ime_offsets() {
        let text = "a\r\n🦊\u{2028}z";
        let normalized = single_line(text);
        assert_eq!(normalized, "a  🦊 z");
        assert_eq!(
            text.encode_utf16().count(),
            normalized.encode_utf16().count()
        );
    }
}
