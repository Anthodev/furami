//! Product top chrome for the GPUI frontend: the Brume ribbon and the
//! immediate selector surfaces.
//!
//! `qml/TopChromeContent.qml`, `SourceSelectorView.qml`,
//! `OutputSelectorView.qml` and `SizeRateSelectorView.qml` are the parity
//! contract. The ribbon shows the confirmed live selection; every selection is
//! applied immediately through the checked `RuntimeCoordinator` intents, so
//! nothing here keeps a pending draft. The host (`gpui.rs` and
//! `companion_view.rs`) owns native geometry, input shape, focus transfers and
//! lifetime; this module renders GPUI chrome only, reports the committed
//! selector-panel rectangle for the X input region, and never invents a second
//! navigation or media policy.
//!
//! Identity rules carried over from the QML views:
//!
//! - Row callbacks bind the opaque row key and the revisions *as rendered*; a
//!   valid stale pair still reaches the runtime guard, which refuses it.
//! - Only the wire-facing decimal tokens are parsed here
//!   (`selectionRevision`, `catalog.revision`, `output.revision`); malformed
//!   values dispatch nothing.
//! - The exact `N` or `N/D` framerate stays unrounded; the only decimal
//!   conversion is the human FPS label, which never feeds a dispatch.
//!
//! Module boundary: everything dispatched from an input event runs through
//! [`HostState`] (the same guarded pattern as `HostState::action()` /
//! `activate()`), because input closures here cannot reach `CompanionView`'s
//! private fields. [`TopChrome`] carries paint-time state and the selector's
//! transient editor buffers.
//!
//! Host integration surface: [`paint`] renders the capture-mode chrome,
//! [`chrome_targets`] feeds the companion's Tab chain, and the companion
//! measures the panel during the same paint.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, ClickEvent, Context, Div, Entity, FocusHandle, Pixels, Role, ScrollHandle,
    SharedString, Size, Stateful, Subscription, Toggled, TransformationMatrix, WeakEntity, Window,
    canvas, div, prelude::*, px,
};

use crate::ui::projection::{Selection, Size as ProjectedSize, Snapshot};

use super::{
    HostState, PendingFullscreen, brume,
    chrome::Popup,
    companion_view::{CompanionView, PanelBounds, measure_panel},
    controls::{Activate, Activation, HitRegions, rate_fps},
    text_field::TextField,
};

/// Popup invoker keys (qml/Chrome.qml `popup` names, top region).
pub(super) const SOURCE_ID: &str = "source";
/// Popup invoker key of the audio output selector.
pub(super) const OUTPUT_ID: &str = "output";
/// Popup invoker key of the resolution selector.
pub(super) const SIZE_ID: &str = "size";
/// Popup invoker key of the framerate selector.
pub(super) const RATE_ID: &str = "rate";

/// Catalog refresh cadence while a source/size/rate popup is open
/// (qml/TopChromeContent.qml timer).
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Ribbon cells wrap to two rows of three below this logical width
/// (`TopChromeContent.compactRibbon`, measured on the ribbon extent, i.e. the
/// client width minus the two window margins).
const COMPACT_RIBBON_MAX: f32 = 660.;
/// Selector surface width: content (`Brume.popupMaxWidth`) plus the popup
/// padding, clamped to the ribbon extent by the caller.
const POPUP_TOTAL_WIDTH: f32 = brume::POPUP_MAX_WIDTH + 2. * brume::EDGE;

/// Which selector surface a popup invoker names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectorKind {
    Source,
    Output,
    Size,
    Rate,
}

impl SelectorKind {
    /// The popup invoker key that opens this surface.
    pub(super) fn invoker(self) -> &'static str {
        match self {
            Self::Source => SOURCE_ID,
            Self::Output => OUTPUT_ID,
            Self::Size => SIZE_ID,
            Self::Rate => RATE_ID,
        }
    }

    /// The surface named by a popup invoker, or `None` for any other popup
    /// (the qualification gate Options surface, future menu/dialog popups).
    pub(super) fn from_invoker(invoker: &'static str) -> Option<Self> {
        match invoker {
            SOURCE_ID => Some(Self::Source),
            OUTPUT_ID => Some(Self::Output),
            SIZE_ID => Some(Self::Size),
            RATE_ID => Some(Self::Rate),
            _ => None,
        }
    }
}

/// One entry of the companion's chrome Tab chain, with the scroll reveal that
/// keeps keyboard focus visible (qml/BrumeScrollView `revealActiveFocus`).
#[derive(Clone, Debug)]
pub(super) struct FocusTarget {
    pub(super) handle: FocusHandle,
    outer: Option<(ScrollHandle, usize)>,
    inner: Option<(ScrollHandle, usize)>,
}

impl FocusTarget {
    pub(super) fn plain(handle: &FocusHandle) -> Self {
        Self {
            handle: handle.clone(),
            outer: None,
            inner: None,
        }
    }
}

/// Nudge the owning scroll view so the focused target is fully visible.
pub(super) fn apply_reveal(target: &FocusTarget) {
    for (scroll, index) in [&target.outer, &target.inner].into_iter().flatten() {
        scroll.scroll_to_item(*index);
    }
}

/// The runtime dispatch a rendered row performs. Every field is bound to the
/// frame that rendered the row: a valid stale pair still reaches the runtime
/// guard, which refuses it; nothing is re-resolved against newer state.
#[derive(Clone, Debug)]
struct RowAction {
    kind: SelectorKind,
    popup: Popup,
    selection_revision: String,
    catalog_revision: String,
    row: String,
    output_revision: Option<u64>,
}

/// Lexical decimal token check, mirroring the producer's `decimal` contract
/// (`crate::ui::catalog::decimal`): `0` or a non-zero-leading digit string,
/// parsed into `u64` without float conversion.
fn decimal_token(value: &str) -> Option<u64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        None
    } else {
        value.parse().ok()
    }
}

/// Same ribbon geometry for the ribbon and the popup surface: extent, height
/// and the compact two-rows-of-three wrap.
fn ribbon_metrics(viewport: Size<Pixels>) -> (f32, f32, bool) {
    let margin = brume::WINDOW_MARGIN;
    let width = (f32::from(viewport.width) - 2. * margin).max(1.);
    let compact = width < COMPACT_RIBBON_MAX;
    let height = if compact {
        brume::RIBBON_HEIGHT * 2.
    } else {
        brume::RIBBON_HEIGHT
    };
    (width, height, compact)
}

fn accepts_chrome(state: &HostState) -> bool {
    state.accepts_input() && state.view.close_dialog.is_empty() && state.overlay_opacity > 0.
}

// ── HostState extension (guarded dispatch, the action()/activate() pattern) ─

impl HostState {
    /// Begin one selector epoch. Opening Source/Size/Rate also refreshes the
    /// catalog immediately and arms the two-second deadline.
    pub(super) fn open_top_selector(&mut self, kind: SelectorKind) -> bool {
        if !accepts_chrome(self) || self.chrome.popup().is_some() || self.view.snapshot.is_none() {
            return false;
        }
        let now = Instant::now();
        self.chrome.open_popup(kind.invoker(), now);
        if kind != SelectorKind::Output {
            dispatch_refresh(self);
            self.top_refresh_due = Some(now + REFRESH_INTERVAL);
        }
        true
    }

    /// Cancel the deadline when Escape or an outside press dismisses a popup.
    pub(super) fn stop_top_refresh(&mut self) {
        self.top_refresh_due = None;
    }

    /// Dispatch one catalog refresh when the two-second schedule is due and a
    /// source/size/rate popup is still open and available.
    pub(super) fn top_refresh_tick(&mut self) {
        let Some(due) = self.top_refresh_due else {
            return;
        };
        let now = Instant::now();
        if now < due {
            return;
        }
        let open = self
            .chrome
            .popup()
            .and_then(|popup| SelectorKind::from_invoker(popup.invoker));
        if matches!(open, None | Some(SelectorKind::Output)) {
            self.top_refresh_due = None;
            return;
        }
        if !accepts_chrome(self) {
            return;
        }
        dispatch_refresh(self);
        self.top_refresh_due = Some(now + REFRESH_INTERVAL);
    }

    /// Fullscreen request from a pointer or keyboard control activation: the
    /// same confirmed native path as F11, without the F11 physical-key latch
    /// that only a real F11 key release clears. May be hoisted into `gpui.rs`.
    pub(super) fn request_fullscreen_from_control(&mut self) -> bool {
        if !self.accepts_input() || !self.fullscreen_restored || self.fullscreen_pending.is_some() {
            return false;
        }
        self.fullscreen_pending = Some(PendingFullscreen::new(
            !self.native_fullscreen,
            true,
            Instant::now(),
        ));
        self.fullscreen_intent = true;
        true
    }
}

/// Dispatch one catalog refresh (qml/Chrome.qml `refreshCatalogs` guards only
/// the closing state; the button adds its own busy gate).
fn dispatch_refresh(state: &mut HostState) {
    let update = state.runtime.refresh_catalogs();
    state.consume(update);
}

/// Dispatch one rendered row through the checked runtime intents, guarding on
/// the rendered popup identity. This is the `HostState::action()`/`activate()`
/// pattern: verify the surface still shows what was clicked, then hand the
/// opaque payload to the runtime and consume the projection.
fn dispatch_row(state: &mut HostState, action: &RowAction) -> bool {
    if state.chrome.popup() != Some(action.popup) || !accepts_chrome(state) {
        return false;
    }
    let update = match action.kind {
        SelectorKind::Source => {
            if decimal_token(&action.selection_revision).is_none()
                || decimal_token(&action.catalog_revision).is_none()
            {
                return false;
            }
            state.runtime.select_capture(
                &action.selection_revision,
                &action.catalog_revision,
                &action.row,
            )
        }
        SelectorKind::Size => {
            if decimal_token(&action.selection_revision).is_none()
                || decimal_token(&action.catalog_revision).is_none()
            {
                return false;
            }
            state.runtime.select_size(
                &action.selection_revision,
                &action.catalog_revision,
                &action.row,
            )
        }
        SelectorKind::Rate => {
            if decimal_token(&action.selection_revision).is_none()
                || decimal_token(&action.catalog_revision).is_none()
            {
                return false;
            }
            state.runtime.select_rate(
                &action.selection_revision,
                &action.catalog_revision,
                &action.row,
            )
        }
        SelectorKind::Output => {
            let Some(revision) = action.output_revision else {
                return false;
            };
            // SubmitStatus intents poll and consume so projected refusals
            // render.
            let _status = state.runtime.select_output(&action.row, revision);
            state.runtime.poll()
        }
    };
    state.consume(update);
    true
}

// ── Activation closures ─────────────────────────────────────────────────────
// Every closure checks its rendered surface before dispatch or focus changes.

fn ribbon_activate(
    state: Rc<RefCell<HostState>>,
    view: WeakEntity<CompanionView>,
    kind: Option<SelectorKind>,
) -> Activate {
    Rc::new(move |source, window, cx| {
        let changed = {
            let mut state = state.borrow_mut();
            if !accepts_chrome(&state) || state.chrome.popup().is_some() {
                return;
            }
            let changed = match kind {
                Some(kind) => state.open_top_selector(kind),
                None => state.request_fullscreen_from_control(),
            };
            if changed {
                state.focus_intent |= source == Activation::Accessibility;
            }
            changed
        };
        if changed && let Some(view) = view.upgrade() {
            view.update(cx, |view, cx| {
                if kind.is_some() {
                    view.focus_selector(source == Activation::Keyboard, window, cx);
                }
                cx.notify();
            });
        }
    })
}

fn row_activation(
    state: Rc<RefCell<HostState>>,
    view: WeakEntity<CompanionView>,
    action: RowAction,
) -> Activate {
    Rc::new(move |source, _window, cx| {
        let _ = view.update(cx, |view, cx| {
            view.finish_mode_editor_for_popup(action.popup, cx)
        });
        {
            let mut state = state.borrow_mut();
            if !dispatch_row(&mut state, &action) {
                return;
            }
            state.focus_intent |= source == Activation::Accessibility;
        }
        let _ = view.update(cx, |_, cx| cx.notify());
    })
}

fn refresh_activation(
    state: Rc<RefCell<HostState>>,
    view: WeakEntity<CompanionView>,
    popup: Popup,
) -> Activate {
    Rc::new(move |source, _window, cx| {
        {
            let mut state = state.borrow_mut();
            if state.chrome.popup() != Some(popup)
                || !accepts_chrome(&state)
                || !state
                    .view
                    .snapshot
                    .as_deref()
                    .is_some_and(|snapshot| !snapshot.catalog.busy && !snapshot.closing)
            {
                return;
            }
            state.focus_intent |= source == Activation::Accessibility;
            dispatch_refresh(&mut state);
        }
        let _ = view.update(cx, |_, cx| cx.notify());
    })
}

// ── Top chrome paint-time state ─────────────────────────────────────────────

/// Top-chrome presentation state owned by `CompanionView`. Holds the fixed
/// ribbon focus handles, the rendered rows' focus handles and the popup
/// surface scroll handles. Everything mutable from an input event lives on
/// [`HostState`] instead, because input closures cannot reach this struct.
pub(super) struct TopChrome {
    source: FocusHandle,
    audio: FocusHandle,
    resolution: FocusHandle,
    rate: FocusHandle,
    fullscreen: FocusHandle,
    popup_root: FocusHandle,
    refresh: FocusHandle,
    /// Focus handles of rendered rows, keyed by their stable element id, so a
    /// catalog refresh that keeps a row keeps its focus node.
    row_handles: RefCell<HashMap<String, FocusHandle>>,
    panel_scroll: ScrollHandle,
    list_scroll: ScrollHandle,
    editor: Option<ModeEditor>,
}

impl TopChrome {
    pub(super) fn new(cx: &mut Context<CompanionView>) -> Self {
        Self {
            source: cx.focus_handle(),
            audio: cx.focus_handle(),
            resolution: cx.focus_handle(),
            rate: cx.focus_handle(),
            fullscreen: cx.focus_handle(),
            popup_root: cx.focus_handle(),
            refresh: cx.focus_handle(),
            row_handles: RefCell::default(),
            panel_scroll: ScrollHandle::new(),
            list_scroll: ScrollHandle::new(),
            editor: None,
        }
    }

    pub(super) fn focused_editor(&self, window: &Window, cx: &App) -> Option<&FocusHandle> {
        self.editor
            .as_ref()
            .filter(|editor| {
                editor.enabled
                    && !editor.group.is_focused(window)
                    && editor.group.contains_focused(window, cx)
            })
            .map(|editor| &editor.group)
    }

    pub(super) fn editor_for_popup(&self, popup: Popup) -> Option<&FocusHandle> {
        self.editor
            .as_ref()
            .filter(|editor| editor.popup == popup)
            .map(|editor| &editor.group)
    }

    pub(super) fn editor_has_field(&self, target: &FocusHandle, cx: &App) -> bool {
        self.editor.as_ref().is_some_and(|editor| {
            std::iter::once(&editor.first)
                .chain(editor.second.iter())
                .any(|field| field.read(cx).handle() == *target)
        })
    }

    pub(super) fn finish_editor(
        &self,
        state: &mut HostState,
        expected: &FocusHandle,
        cx: &mut App,
    ) {
        if let Some(editor) = &self.editor
            && editor.group == *expected
        {
            editor.finish(state, cx);
        }
    }
}

#[derive(PartialEq)]
enum ProjectedMode {
    Size(Option<ProjectedSize>),
    Rate(Option<String>),
}

struct ModeEditor {
    popup: Popup,
    source: String,
    format_policy: &'static str,
    manual_format: String,
    // Interval descriptors depend on the selected size, but size descriptors
    // do not depend on the rate.
    interval_size: Option<ProjectedSize>,
    projected: ProjectedMode,
    revisions: (String, String),
    enabled: bool,
    group: FocusHandle,
    first: Entity<TextField>,
    second: Option<Entity<TextField>>,
    _blur: Subscription,
}

impl ModeEditor {
    fn values(projected: &ProjectedMode) -> (String, Option<String>) {
        match projected {
            ProjectedMode::Size(size) => (
                size.map_or_else(String::new, |size| size.width.to_string()),
                Some(size.map_or_else(String::new, |size| size.height.to_string())),
            ),
            ProjectedMode::Rate(rate) => (rate.clone().unwrap_or_default(), None),
        }
    }

    fn new(
        popup: Popup,
        selection: Option<&Selection>,
        size_mode: bool,
        window: &mut Window,
        cx: &mut Context<CompanionView>,
    ) -> Self {
        let group = cx.focus_handle();
        let observed = group.clone();
        let blur = cx.on_focus_out(&group, window, move |view, _, _, cx| {
            view.finish_mode_editor(&observed, cx);
        });
        let projected = Self::projected(selection, size_mode);
        let (first, second) = Self::values(&projected);
        let first = cx.new(|cx| {
            TextField::new(
                if size_mode {
                    "Width in pixels"
                } else {
                    "Exact framerate: an integer or N/D"
                },
                first,
                cx,
            )
        });
        let second =
            second.map(|height| cx.new(|cx| TextField::new("Height in pixels", height, cx)));
        Self {
            popup,
            source: selection.map_or_else(String::new, |s| s.source_key.clone()),
            format_policy: selection.map_or("", |s| s.format_policy),
            manual_format: selection.map_or_else(String::new, |s| s.manual_format_key.clone()),
            interval_size: Self::interval_size(selection, size_mode),
            projected,
            revisions: (String::new(), String::new()),
            enabled: true,
            group,
            first,
            second,
            _blur: blur,
        }
    }

    fn interval_size(selection: Option<&Selection>, size_mode: bool) -> Option<ProjectedSize> {
        selection.filter(|_| !size_mode).and_then(|s| s.size)
    }

    fn projected(selection: Option<&Selection>, size_mode: bool) -> ProjectedMode {
        if size_mode {
            ProjectedMode::Size(selection.and_then(|s| s.size))
        } else {
            ProjectedMode::Rate(
                selection
                    .and_then(|s| s.rate.as_ref())
                    .map(|r| r.text.clone()),
            )
        }
    }

    fn paint(
        slot: &mut Option<Self>,
        popup: Popup,
        snapshot: &Snapshot,
        size_mode: bool,
        window: &mut Window,
        cx: &mut Context<CompanionView>,
    ) -> Stateful<Div> {
        let selection = snapshot.selection.as_ref();
        if slot.as_ref().is_some_and(|editor| {
            editor.popup != popup
                || editor.source != selection.map_or("", |s| s.source_key.as_str())
                || editor.format_policy != selection.map_or("", |s| s.format_policy)
                || editor.manual_format != selection.map_or("", |s| s.manual_format_key.as_str())
                || editor.interval_size != Self::interval_size(selection, size_mode)
        }) {
            *slot = None;
        }
        let editor = slot.get_or_insert_with(|| Self::new(popup, selection, size_mode, window, cx));
        let projected_changed = match &editor.projected {
            ProjectedMode::Size(size) => *size != selection.and_then(|s| s.size),
            ProjectedMode::Rate(rate) => {
                rate.as_deref()
                    != selection
                        .and_then(|s| s.rate.as_ref())
                        .map(|r| r.text.as_str())
            }
        };
        if projected_changed {
            editor.projected = Self::projected(selection, size_mode);
            let (first, second) = Self::values(&editor.projected);
            editor.first.update(cx, |field, cx| field.reset(first, cx));
            if let Some((field, value)) = editor.second.as_ref().zip(second) {
                field.update(cx, |field, cx| field.reset(value, cx));
            }
        }
        let enabled = snapshot.can_edit_selection && selection.is_some();
        if editor.enabled != enabled {
            editor.enabled = enabled;
            for field in std::iter::once(&editor.first).chain(editor.second.iter()) {
                field.update(cx, |field, cx| field.set_enabled(enabled, cx));
            }
        }
        editor.revisions.0.clone_from(&snapshot.selection_revision);
        editor.revisions.1.clone_from(&snapshot.catalog.revision);
        let field = |title, entity: &Entity<TextField>| {
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(brume::GAP_SMALL))
                .child(label(title, LabelVariant::Secondary, enabled))
                .child(entity.clone())
        };
        div()
            .id("mode-editor-group")
            .tab_stop(false)
            .track_focus(&editor.group)
            .w_full()
            .flex()
            .gap(px(brume::GAP_SMALL))
            .child(field(
                if size_mode {
                    "Width"
                } else {
                    "Framerate (e.g. 30000/1001)"
                },
                &editor.first,
            ))
            .children(editor.second.as_ref().map(|entity| field("Height", entity)))
    }

    fn finish(&self, state: &mut HostState, cx: &mut App) {
        if !self.enabled
            || state.chrome.popup() != Some(self.popup)
            || !accepts_chrome(state)
            || decimal_token(&self.revisions.0).is_none()
            || decimal_token(&self.revisions.1).is_none()
        {
            return;
        }
        let first = self.first.read(cx);
        let second = self.second.as_ref().map(|field| field.read(cx));
        if first.is_composing()
            || second.is_some_and(TextField::is_composing)
            || !(first.is_dirty() || second.is_some_and(TextField::is_dirty))
        {
            return;
        }
        let valid = match second {
            Some(height) => positive_u32(first.text()) && positive_u32(height.text()),
            None => valid_exact_rate(first.text()),
        };
        if !valid {
            return;
        }
        // Consume the genuine edit before calling the runtime: Enter followed
        // by blur (or a refusal) must not submit it again.
        let first = self.first.update(cx, |field, _| field.take_edit());
        let second = self
            .second
            .as_ref()
            .map(|field| field.update(cx, |field, _| field.take_edit()));
        let (revision, catalog) = &self.revisions;
        let update = if let Some(height) = second {
            state.runtime.set_size(revision, catalog, &first, &height)
        } else {
            state.runtime.set_rate(revision, catalog, &first)
        };
        state.consume(update);
    }
}

fn positive_u32(text: &str) -> bool {
    text.bytes().all(|byte| byte.is_ascii_digit())
        && text.parse::<u32>().is_ok_and(|value| value != 0)
}

fn valid_exact_rate(text: &str) -> bool {
    let (numerator, denominator) = text.split_once('/').unwrap_or((text, "1"));
    positive_u32(numerator) && positive_u32(denominator)
}

/// The ribbon focus handle a selector popup returns focus to when it is
/// dismissed (qml/TopChromeContent `invoker` restore).
pub(super) fn restore_handle(top: &TopChrome, invoker: &'static str) -> Option<FocusHandle> {
    let handle = match invoker {
        SOURCE_ID => &top.source,
        OUTPUT_ID => &top.audio,
        SIZE_ID => &top.resolution,
        RATE_ID => &top.rate,
        _ => return None,
    };
    Some(handle.clone())
}

/// The popup panel's own focus handle, used when a selector surface has no
/// reachable control (qml/BrumePopup::focusBoundary falls back to the content
/// item).
pub(super) fn popup_root(top: &TopChrome) -> FocusHandle {
    top.popup_root.clone()
}

/// The chrome Tab chain contributed by the top chrome: the five ribbon cells
/// when no popup is open, otherwise the open selector surface's reachable
/// controls in visual order. Entries the companion appends after these (the
/// volume rail, later the bottom tool) must only be added while no popup is
/// open, matching their paint conditions.
pub(super) fn chrome_targets(top: &TopChrome, state: &HostState, cx: &App) -> Vec<FocusTarget> {
    if !accepts_chrome(state) {
        return Vec::new();
    }
    match state
        .chrome
        .popup()
        .and_then(|popup| SelectorKind::from_invoker(popup.invoker))
    {
        Some(kind) => popup_targets(top, state, kind, cx),
        None => ribbon_targets(top, state),
    }
}

fn ribbon_targets(top: &TopChrome, state: &HostState) -> Vec<FocusTarget> {
    // Projection.ready gates every ribbon control.
    if state.view.snapshot.is_none() {
        return Vec::new();
    }
    vec![
        FocusTarget::plain(&top.source),
        FocusTarget::plain(&top.audio),
        FocusTarget::plain(&top.resolution),
        FocusTarget::plain(&top.rate),
        FocusTarget::plain(&top.fullscreen),
    ]
}

fn popup_targets(
    top: &TopChrome,
    state: &HostState,
    kind: SelectorKind,
    cx: &App,
) -> Vec<FocusTarget> {
    let Some(snapshot) = state.view.snapshot.as_deref() else {
        return Vec::new();
    };
    let mut targets = Vec::new();
    // The title is the first child; notices preceding the list are also
    // direct children of the scrollable panel body.
    let list_index = match kind {
        SelectorKind::Source => {
            1 + usize::from(!snapshot.catalog.error.is_empty())
                + usize::from(!snapshot.catalog.capability_reason.is_empty())
        }
        SelectorKind::Output => 1,
        SelectorKind::Size | SelectorKind::Rate => {
            1 + usize::from(!snapshot.catalog.error.is_empty())
        }
    };
    let row_target = |key: &str, index| FocusTarget {
        handle: row_handle(top, key, cx),
        outer: Some((top.panel_scroll.clone(), list_index)),
        inner: Some((top.list_scroll.clone(), index)),
    };
    match kind {
        SelectorKind::Source => {
            // The refresh button is enabled only while the catalog is not
            // busy and the application is not closing.
            if !snapshot.catalog.busy && !snapshot.closing {
                targets.push(FocusTarget {
                    handle: top.refresh.clone(),
                    outer: Some((top.panel_scroll.clone(), 0)),
                    inner: None,
                });
            }
            for (index, row) in snapshot.catalog.source_rows.iter().enumerate() {
                if snapshot.can_edit_selection && row.eligible {
                    targets.push(row_target(&row.key, index));
                }
            }
        }
        SelectorKind::Output => {
            for (index, row) in snapshot.output.rows.iter().enumerate() {
                if row.eligible {
                    targets.push(row_target(&row.key, index));
                }
            }
        }
        SelectorKind::Size => {
            let editable = snapshot.can_edit_selection && snapshot.selection.is_some();
            for (index, row) in snapshot.catalog.size_rows.iter().enumerate() {
                if editable && row.eligible {
                    targets.push(row_target(&row.key, index));
                }
            }
        }
        SelectorKind::Rate => {
            let editable = snapshot.can_edit_selection && snapshot.selection.is_some();
            for (index, row) in snapshot.catalog.rate_rows.iter().enumerate() {
                if editable && row.eligible {
                    targets.push(row_target(&row.key, index));
                }
            }
        }
    }
    if let Some(editor) = &top.editor
        && Some(editor.popup) == state.chrome.popup()
        && editor.enabled
    {
        let rows_empty = match kind {
            SelectorKind::Size => snapshot.catalog.size_rows.is_empty(),
            SelectorKind::Rate => snapshot.catalog.rate_rows.is_empty(),
            _ => true,
        };
        for field in std::iter::once(&editor.first).chain(editor.second.iter()) {
            targets.push(FocusTarget {
                handle: field.read(cx).handle(),
                outer: Some((
                    top.panel_scroll.clone(),
                    list_index + usize::from(!rows_empty) + 1,
                )),
                inner: None,
            });
        }
    }
    targets
}

/// Get-or-create the focus handle of one rendered row.
fn row_handle(top: &TopChrome, key: &str, cx: &App) -> FocusHandle {
    let mut handles = top.row_handles.borrow_mut();
    handles
        .entry(key.to_owned())
        .or_insert_with(|| cx.focus_handle())
        .clone()
}

/// Drop focus handles of rows that are not part of the current list.
fn retain_rows(top: &TopChrome, keys: &[String]) {
    top.row_handles
        .borrow_mut()
        .retain(|key, _| keys.contains(key));
}

// ── Painting ────────────────────────────────────────────────────────────────

/// Render the capture-mode top chrome: the Brume ribbon and, while a selector
/// popup is open, its surface. The qualification gate keeps its own verified
/// companion rendering; `enabled` is the companion's `accepts_chrome` fact.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint(
    top: &mut TopChrome,
    host: &Rc<RefCell<HostState>>,
    popup: Option<Popup>,
    enabled: bool,
    panel: Option<&PanelBounds>,
    regions: &HitRegions,
    window: &mut Window,
    cx: &mut Context<CompanionView>,
) -> Div {
    if top
        .editor
        .as_ref()
        .is_some_and(|editor| Some(editor.popup) != popup)
    {
        top.editor = None;
    }
    if !enabled {
        // Mandatory decisions paint no top chrome, matching the QML tool
        // visibility while a session dialog owns the window.
        return div();
    }
    let mut root = div().child(paint_ribbon(
        top,
        host,
        popup.is_none(),
        regions,
        window,
        cx,
    ));
    if let Some(popup) = popup
        && let Some(kind) = SelectorKind::from_invoker(popup.invoker)
    {
        let state = host.borrow();
        if let Some(snapshot) = state.view.snapshot.as_deref() {
            root = root.child(paint_popup(
                top, host, kind, popup, snapshot, panel, regions, window, cx,
            ));
        }
    }
    root
}

fn paint_ribbon(
    top: &mut TopChrome,
    host: &Rc<RefCell<HostState>>,
    enabled: bool,
    regions: &HitRegions,
    window: &mut Window,
    cx: &mut Context<CompanionView>,
) -> Div {
    let viewport = window.viewport_size();
    let (width, height, compact) = ribbon_metrics(viewport);
    let weak = cx.weak_entity();
    let state = host.borrow();
    let Some(snapshot) = state.view.snapshot.as_deref() else {
        // No projection yet: the disabled ribbon still paints its cells
        // (qml/TopChromeContent, `enabled: Projection.ready`).
        let source = ribbon_cell(
            "ribbon-source",
            "camera",
            "Source",
            "—".into(),
            true,
            false,
            "Capture source".into(),
            &top.source,
            regions,
            None,
        );
        let audio = ribbon_cell(
            "ribbon-output",
            "headphones",
            "Audio",
            "Auto".into(),
            true,
            false,
            "Playback audio output".into(),
            &top.audio,
            regions,
            None,
        );
        let size = ribbon_cell(
            "ribbon-resolution",
            "maximize",
            "Resolution",
            "—".into(),
            true,
            false,
            "Capture resolution".into(),
            &top.resolution,
            regions,
            None,
        );
        let rate = ribbon_cell(
            "ribbon-rate",
            "refresh",
            "Framerate",
            "—".into(),
            true,
            false,
            "Framerate".into(),
            &top.rate,
            regions,
            None,
        );
        let fullscreen = ribbon_cell(
            "ribbon-fullscreen",
            "maximize",
            "Window",
            "Windowed".into(),
            false,
            false,
            "Enter full screen".into(),
            &top.fullscreen,
            regions,
            None,
        );
        return ribbon_surface(
            width,
            height,
            compact,
            source.into_any_element(),
            audio.into_any_element(),
            size.into_any_element(),
            rate.into_any_element(),
            fullscreen.into_any_element(),
        );
    };
    let fullscreen = state.view.fullscreen;
    let fullscreen_value: SharedString = if fullscreen {
        "Full screen".into()
    } else {
        "Windowed".into()
    };
    let fullscreen_icon = if fullscreen { "minimize" } else { "maximize" };
    let fullscreen_a11y: SharedString = if fullscreen {
        "Exit full screen".into()
    } else {
        "Enter full screen".into()
    };
    let source_value: SharedString = if snapshot.live.source_label.is_empty() {
        "—".into()
    } else {
        snapshot.live.source_label.clone().into()
    };
    let audio_value: SharedString = if snapshot.output.selected_label.is_empty() {
        "Auto".into()
    } else {
        snapshot.output.selected_label.clone().into()
    };
    let size_value: SharedString = if snapshot.live.size_label.is_empty() {
        "—".into()
    } else {
        snapshot.live.size_label.clone().into()
    };
    let rate_value: SharedString = if snapshot.live.rate_label.is_empty() {
        "—".into()
    } else {
        rate_fps(&snapshot.live.rate_label).into()
    };
    let source = ribbon_cell(
        "ribbon-source",
        "camera",
        "Source",
        source_value,
        true,
        enabled,
        "Capture source".into(),
        &top.source,
        regions,
        enabled.then(|| ribbon_activate(Rc::clone(host), weak.clone(), Some(SelectorKind::Source))),
    );
    let audio = ribbon_cell(
        "ribbon-output",
        "headphones",
        "Audio",
        audio_value,
        true,
        enabled,
        "Playback audio output".into(),
        &top.audio,
        regions,
        enabled.then(|| ribbon_activate(Rc::clone(host), weak.clone(), Some(SelectorKind::Output))),
    );
    let size = ribbon_cell(
        "ribbon-resolution",
        "maximize",
        "Resolution",
        size_value,
        true,
        enabled,
        "Capture resolution".into(),
        &top.resolution,
        regions,
        enabled.then(|| ribbon_activate(Rc::clone(host), weak.clone(), Some(SelectorKind::Size))),
    );
    let rate = ribbon_cell(
        "ribbon-rate",
        "refresh",
        "Framerate",
        rate_value,
        true,
        enabled,
        "Framerate".into(),
        &top.rate,
        regions,
        enabled.then(|| ribbon_activate(Rc::clone(host), weak.clone(), Some(SelectorKind::Rate))),
    );
    let fullscreen_cell = ribbon_cell(
        "ribbon-fullscreen",
        fullscreen_icon,
        "Window",
        fullscreen_value,
        false,
        enabled,
        fullscreen_a11y,
        &top.fullscreen,
        regions,
        enabled.then(|| ribbon_activate(Rc::clone(host), weak, None)),
    );
    ribbon_surface(
        width,
        height,
        compact,
        source.into_any_element(),
        audio.into_any_element(),
        size.into_any_element(),
        rate.into_any_element(),
        fullscreen_cell.into_any_element(),
    )
}

#[allow(clippy::too_many_arguments)]
fn ribbon_surface(
    width: f32,
    height: f32,
    compact: bool,
    source: AnyElement,
    audio: AnyElement,
    size: AnyElement,
    rate: AnyElement,
    fullscreen: AnyElement,
) -> Div {
    // Row-major like the QML GridLayout: five columns, or two rows of three
    // cells when the ribbon extent is below the compact threshold; the sixth
    // cell stays empty.
    let row = |cells: Vec<AnyElement>| {
        div()
            .w_full()
            .h(px(brume::RIBBON_HEIGHT))
            .flex()
            .children(cells)
            .into_any_element()
    };
    let grid = if compact {
        div().flex().flex_col().children([
            row(vec![source, audio, size]),
            row(vec![rate, fullscreen, div().flex_1().into_any_element()]),
        ])
    } else {
        div()
            .flex()
            .w_full()
            .children([row(vec![source, audio, size, rate, fullscreen])])
    };
    div()
        .absolute()
        .left(px(brume::WINDOW_MARGIN))
        .top(px(brume::WINDOW_MARGIN))
        .w(px(width))
        .h(px(height))
        .child(
            grid.rounded(px(brume::RADIUS_SURFACE))
                .bg(brume::SURFACE)
                .border_1()
                .border_color(brume::DIVIDER),
        )
}

#[allow(clippy::too_many_arguments)]
fn ribbon_cell(
    id: &'static str,
    icon: &'static str,
    caption: &'static str,
    value: SharedString,
    chevron: bool,
    enabled: bool,
    a11y: SharedString,
    focus: &FocusHandle,
    regions: &HitRegions,
    activate: Option<Activate>,
) -> Stateful<Div> {
    let icon_tint = if enabled {
        brume::SECONDARY
    } else {
        brume::DISABLED_TEXT
    };
    let caption_color = if enabled {
        brume::SECONDARY
    } else {
        brume::DISABLED_TEXT
    };
    let value_color = if enabled {
        brume::FOREGROUND
    } else {
        brume::SECONDARY
    };
    let content = div()
        .flex_1()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(
            div()
                .w_full()
                .truncate()
                .text_size(px(brume::FONT_SIZE_SMALL))
                .text_color(caption_color)
                .child(caption),
        )
        .child(
            div()
                .w_full()
                .truncate()
                .text_size(px(brume::FONT_SIZE))
                .text_color(value_color)
                .child(value),
        );
    let mut element = div()
        .id(id)
        .flex_1()
        .h(px(brume::RIBBON_HEIGHT))
        .flex()
        .items_center()
        .pl(px(brume::EDGE))
        .pr(px(brume::GAP))
        .gap(px(brume::GAP))
        .rounded(px(brume::RADIUS_CONTROL))
        .text_size(px(brume::FONT_SIZE))
        .role(Role::Button)
        .aria_label(a11y)
        .tab_stop(enabled)
        .when(enabled, |cell| cell.track_focus(focus))
        .child(glyph(brume::icon(icon), brume::RIBBON_ICON_SIZE, icon_tint))
        .child(content)
        .when(chevron, |cell| {
            cell.child(glyph(brume::icon("chevron-down"), 14., icon_tint))
        });
    if enabled {
        element = element
            .hover(|style| style.bg(brume::CONTROL_HOVER))
            .active(|style| style.bg(brume::CONTROL_PRESSED));
        element = element.child(hit_layer(regions));
        if let Some(activate) = activate {
            element = with_activation(element, activate);
        }
    }
    element.focus_visible(|style| style.border_2().border_color(brume::ACCENT))
}

#[allow(clippy::too_many_arguments)]
fn paint_popup(
    top: &mut TopChrome,
    host: &Rc<RefCell<HostState>>,
    kind: SelectorKind,
    popup: Popup,
    snapshot: &Snapshot,
    panel: Option<&PanelBounds>,
    regions: &HitRegions,
    window: &mut Window,
    cx: &mut Context<CompanionView>,
) -> Stateful<Div> {
    let viewport = window.viewport_size();
    let (_, ribbon_height, _) = ribbon_metrics(viewport);
    let margin = brume::WINDOW_MARGIN;
    let panel_width = POPUP_TOTAL_WIDTH.min((f32::from(viewport.width) - 2. * margin).max(1.));
    let panel_max_height =
        (f32::from(viewport.height) - 2. * margin - ribbon_height - brume::GAP).max(1.);
    let title = match kind {
        SelectorKind::Source => "Capture source",
        SelectorKind::Output => "Audio output",
        SelectorKind::Size => "Resolution",
        SelectorKind::Rate => "Framerate",
    };
    div()
        .id("top-selector-popup")
        .absolute()
        .left(px(margin))
        .top(px(margin + ribbon_height + brume::GAP))
        .w(px(panel_width))
        .max_h(px(panel_max_height))
        .rounded(px(brume::RADIUS_SURFACE))
        .bg(brume::MENU_SURFACE)
        .border_1()
        .border_color(brume::DIVIDER)
        .role(Role::Group)
        .aria_label(title)
        .tab_stop(false)
        .track_focus(&top.popup_root)
        .children(panel.map(|panel| measure_panel(Rc::clone(panel))))
        .child(
            paint_selector(top, host, kind, popup, snapshot, regions, window, cx)
                .max_h(px(panel_max_height))
                .p(px(brume::EDGE))
                .overflow_y_scroll()
                .track_scroll(&top.panel_scroll),
        )
}

#[allow(clippy::too_many_arguments)]
fn paint_selector(
    top: &mut TopChrome,
    host: &Rc<RefCell<HostState>>,
    kind: SelectorKind,
    popup: Popup,
    snapshot: &Snapshot,
    regions: &HitRegions,
    window: &mut Window,
    cx: &mut Context<CompanionView>,
) -> Stateful<Div> {
    let weak = cx.weak_entity();
    let mut panel = div()
        .id("top-selector-body")
        .flex()
        .flex_col()
        .gap(px(brume::GAP));
    match kind {
        SelectorKind::Source => {
            let refreshable = !snapshot.catalog.busy && !snapshot.closing;
            panel = panel.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(brume::GAP))
                    .child(
                        label("Capture source", LabelVariant::Secondary, true)
                            .flex_1()
                            .min_w(px(0.)),
                    )
                    .child(icon_button(
                        "source-refresh",
                        "refresh",
                        "Refresh catalogs",
                        refreshable,
                        &top.refresh,
                        regions,
                        refreshable
                            .then(|| refresh_activation(Rc::clone(host), weak.clone(), popup)),
                    )),
            );
            if !snapshot.catalog.error.is_empty() {
                panel = panel.child(
                    label(snapshot.catalog.error.clone(), LabelVariant::Error, true).w_full(),
                );
            }
            if !snapshot.catalog.capability_reason.is_empty() {
                panel = panel.child(
                    label(
                        format!(
                            "Capabilities: {} — {}",
                            snapshot.catalog.capability_status, snapshot.catalog.capability_reason
                        ),
                        LabelVariant::Note,
                        true,
                    )
                    .w_full(),
                );
            }
            let keys: Vec<String> = snapshot
                .catalog
                .source_rows
                .iter()
                .map(|row| row.key.clone())
                .collect();
            retain_rows(top, &keys);
            let rows: Vec<RowModel> = snapshot
                .catalog
                .source_rows
                .iter()
                .map(|row| RowModel {
                    key: row.key.clone(),
                    caption: row.label.clone(),
                    detail: None,
                    reason: row.reason.clone(),
                    show_reason: false,
                    // The column's enabled cascade gates every row.
                    eligible: snapshot.can_edit_selection && row.eligible,
                    selected: row.key == snapshot.catalog.selected_source_key,
                })
                .collect();
            if rows.is_empty() {
                panel = panel.child(label(
                    if snapshot.catalog.busy {
                        "Discovering sources…".to_owned()
                    } else {
                        "No source reported by the catalog yet.".to_owned()
                    },
                    LabelVariant::Note,
                    true,
                ));
            } else {
                let painted = paint_rows(top, host, rows, kind, popup, snapshot, cx);
                panel = panel.child(list_surface(top, painted));
            }
        }
        SelectorKind::Output => {
            panel = panel.child(label("Audio output", LabelVariant::Secondary, true).w_full());
            let keys: Vec<String> = snapshot
                .output
                .rows
                .iter()
                .map(|row| row.key.clone())
                .collect();
            retain_rows(top, &keys);
            let rows: Vec<RowModel> = snapshot
                .output
                .rows
                .iter()
                .map(|row| RowModel {
                    key: row.key.clone(),
                    caption: row.label.clone(),
                    detail: None,
                    reason: row.reason.clone(),
                    show_reason: false,
                    eligible: row.eligible,
                    selected: row.key == snapshot.output.selected_key,
                })
                .collect();
            if !rows.is_empty() {
                let painted = paint_rows(top, host, rows, kind, popup, snapshot, cx);
                panel = panel.child(list_surface(top, painted));
            }
            if snapshot.output.selected_key.is_empty() && !snapshot.output.selected_label.is_empty()
            {
                panel = panel.child(
                    label(
                        format!(
                            "Selected output \"{}\" not found: no automatic substitution.",
                            snapshot.output.selected_label
                        ),
                        LabelVariant::Error,
                        true,
                    )
                    .w_full(),
                );
            }
            if !snapshot.output.status.is_empty() {
                panel = panel.child(output_status_label(snapshot));
            }
        }
        SelectorKind::Size | SelectorKind::Rate => {
            let size_mode = kind == SelectorKind::Size;
            panel = panel.child(
                label(
                    if size_mode { "Resolution" } else { "Framerate" },
                    LabelVariant::Title,
                    true,
                )
                .w_full(),
            );
            if !snapshot.catalog.error.is_empty() {
                panel = panel.child(
                    label(snapshot.catalog.error.clone(), LabelVariant::Error, true).w_full(),
                );
            }
            let editable = snapshot.can_edit_selection && snapshot.selection.is_some();
            let rows: Vec<RowModel> = if size_mode {
                snapshot
                    .catalog
                    .size_rows
                    .iter()
                    .map(|row| {
                        let selected = snapshot
                            .selection
                            .as_ref()
                            .and_then(|selection| selection.size.as_ref())
                            .is_some_and(|size| {
                                size.width == row.width && size.height == row.height
                            });
                        RowModel {
                            key: row.key.clone(),
                            caption: row.label.clone(),
                            detail: row_detail(
                                &row.label,
                                &format!("{} × {}", row.width, row.height),
                            ),
                            reason: row.reason.clone(),
                            show_reason: true,
                            eligible: editable && row.eligible,
                            selected,
                        }
                    })
                    .collect()
            } else {
                snapshot
                    .catalog
                    .rate_rows
                    .iter()
                    .map(|row| {
                        let selected = snapshot
                            .selection
                            .as_ref()
                            .and_then(|selection| selection.rate.as_ref())
                            .is_some_and(|rate| rate.text == row.exact);
                        RowModel {
                            key: row.key.clone(),
                            caption: rate_fps(&row.exact),
                            detail: row_detail(&row.label, &row.exact),
                            reason: row.reason.clone(),
                            show_reason: true,
                            eligible: editable && row.eligible,
                            selected,
                        }
                    })
                    .collect()
            };
            let keys: Vec<String> = rows.iter().map(|row| row.key.clone()).collect();
            retain_rows(top, &keys);
            // The producer already filters by the policy and runtime support;
            // a manual format keeps only that format's real ranges, and the
            // empty note sees the filtered list like the QML view does.
            let range_texts: Vec<String> = if size_mode {
                filtered_ranges(&snapshot.catalog.size_ranges, snapshot)
                    .iter()
                    .map(|range| size_range_text(snapshot, range))
                    .collect()
            } else {
                filtered_intervals(&snapshot.catalog.interval_ranges, snapshot)
                    .iter()
                    .map(|range| interval_range_text(snapshot, range))
                    .collect()
            };
            if rows.is_empty() {
                if range_texts.is_empty() {
                    panel = panel.child(label(
                        if snapshot.catalog.busy {
                            "Reading formats…".to_owned()
                        } else if snapshot.catalog.capability_reason.is_empty() {
                            "No mode reported for the selected node.".to_owned()
                        } else {
                            snapshot.catalog.capability_reason.clone()
                        },
                        LabelVariant::Note,
                        true,
                    ));
                }
            } else {
                let painted = paint_rows(top, host, rows, kind, popup, snapshot, cx);
                panel = panel.child(list_surface(top, painted));
            }
            if !range_texts.is_empty() {
                panel = panel.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(brume::GAP_SMALL))
                        .children(
                            range_texts
                                .into_iter()
                                .map(|text| label(text, LabelVariant::Secondary, true).w_full()),
                        ),
                );
                panel = panel.child(ModeEditor::paint(
                    &mut top.editor,
                    popup,
                    snapshot,
                    size_mode,
                    window,
                    cx,
                ));
            } else {
                top.editor = None;
            }
            panel = panel.child(label(
                if size_mode {
                    "A value outside the descriptor is refused by the engine's exact validation, never rounded."
                } else {
                    "Discrete rows are exact framerates; the native range is a period in seconds, never converted to frames/s. A framerate outside the descriptor is refused, never rounded."
                },
                LabelVariant::Note,
                true,
            ));
        }
    }
    panel
}

struct RowModel {
    key: String,
    caption: String,
    detail: Option<String>,
    reason: String,
    show_reason: bool,
    eligible: bool,
    selected: bool,
}

fn paint_rows(
    top: &mut TopChrome,
    host: &Rc<RefCell<HostState>>,
    rows: Vec<RowModel>,
    kind: SelectorKind,
    popup: Popup,
    snapshot: &Snapshot,
    cx: &mut Context<CompanionView>,
) -> Vec<AnyElement> {
    let weak = cx.weak_entity();
    rows.into_iter()
        .map(|row| {
            // The activation node is frame-bound even when GPUI keeps the same
            // logical focus handle. A repaint must not retarget a pending click.
            let name = if kind == SelectorKind::Output {
                format!("{popup:?}-row-{}-{}", snapshot.output.revision, row.key)
            } else {
                format!(
                    "{popup:?}-row-{}-{}-{}",
                    snapshot.selection_revision, snapshot.catalog.revision, row.key
                )
            };
            let id = format!("{popup:?}:{}:{name}", popup.epoch);
            let activate = row_action(kind, popup, snapshot, &row)
                .map(|action| row_activation(Rc::clone(host), weak.clone(), action));
            choice_row(
                (id, name),
                row.caption.into(),
                row.detail.map(SharedString::from),
                row.reason.into(),
                row.show_reason,
                row.eligible,
                row.selected,
                row_handle(top, &row.key, cx),
                activate,
            )
            .into_any_element()
        })
        .collect()
}

fn row_action(
    kind: SelectorKind,
    popup: Popup,
    snapshot: &Snapshot,
    row: &RowModel,
) -> Option<RowAction> {
    match kind {
        SelectorKind::Source | SelectorKind::Size | SelectorKind::Rate => {
            let selection_revision = snapshot.selection_revision.clone();
            let catalog_revision = snapshot.catalog.revision.clone();
            (decimal_token(&selection_revision).is_some()
                && decimal_token(&catalog_revision).is_some())
            .then_some(RowAction {
                kind,
                popup,
                selection_revision,
                catalog_revision,
                row: row.key.clone(),
                output_revision: None,
            })
        }
        SelectorKind::Output => {
            decimal_token(&snapshot.output.revision).map(|output_revision| RowAction {
                kind,
                popup,
                selection_revision: String::new(),
                catalog_revision: String::new(),
                row: row.key.clone(),
                output_revision: Some(output_revision),
            })
        }
    }
}

fn list_surface(top: &TopChrome, rows: Vec<AnyElement>) -> Stateful<Div> {
    div()
        .id("top-selector-list")
        .w_full()
        .max_h(px(brume::POPUP_MAX_LIST_HEIGHT))
        .flex()
        .flex_col()
        .gap(px(brume::GAP_SMALL))
        .overflow_y_scroll()
        .track_scroll(&top.list_scroll)
        .children(rows)
}

/// A row detail only repeats the numeric value when it differs from the
/// caption: identical numbers are never shown twice.
fn row_detail(label: &str, extra: &str) -> Option<String> {
    (extra != label).then(|| extra.to_owned())
}

fn filtered_ranges<'a>(
    ranges: &'a [crate::ui::projection::SizeRange],
    snapshot: &'a Snapshot,
) -> Vec<&'a crate::ui::projection::SizeRange> {
    let manual_key = snapshot
        .selection
        .as_ref()
        .filter(|selection| selection.format_policy == "manual")
        .map(|selection| selection.manual_format_key.as_str());
    match manual_key {
        Some(key) => ranges
            .iter()
            .filter(|range| range.format_key == key)
            .collect(),
        None => ranges.iter().collect(),
    }
}

fn filtered_intervals<'a>(
    ranges: &'a [crate::ui::projection::IntervalRange],
    snapshot: &'a Snapshot,
) -> Vec<&'a crate::ui::projection::IntervalRange> {
    let manual_key = snapshot
        .selection
        .as_ref()
        .filter(|selection| selection.format_policy == "manual")
        .map(|selection| selection.manual_format_key.as_str());
    match manual_key {
        Some(key) => ranges
            .iter()
            .filter(|range| range.format_key == key)
            .collect(),
        None => ranges.iter().collect(),
    }
}

fn format_label(snapshot: &Snapshot, format_key: &str) -> String {
    snapshot
        .catalog
        .format_rows
        .iter()
        .find(|row| row.key == format_key)
        .map(|row| row.label.clone())
        .unwrap_or_else(|| format_key.to_owned())
}

fn size_range_text(snapshot: &Snapshot, range: &crate::ui::projection::SizeRange) -> String {
    let kind = if range.kind == "continuous" {
        "Continuous range: "
    } else {
        "Range: "
    };
    let step = range
        .step_width
        .map(|width| format!("   step {} × {}", width, range.step_height.unwrap_or(0)))
        .unwrap_or_default();
    format!(
        "{} · {}{} — {}{}",
        format_label(snapshot, &range.format_key),
        kind,
        size_text(&range.min),
        size_text(&range.max),
        step
    )
}

fn interval_range_text(
    snapshot: &Snapshot,
    range: &crate::ui::projection::IntervalRange,
) -> String {
    let kind = if range.kind == "continuous" {
        "Native continuous interval (period): "
    } else {
        "Native interval (period): "
    };
    let step = range
        .step
        .map(|step| format!("   step {}", rational_text(&step)))
        .unwrap_or_default();
    format!(
        "{} · {}{} — {}{}",
        format_label(snapshot, &range.format_key),
        kind,
        rational_text(&range.min),
        rational_text(&range.max),
        step
    )
}

fn size_text(size: &crate::ui::projection::Size) -> String {
    format!("{} × {}", size.width, size.height)
}

fn rational_text(rational: &crate::ui::projection::Rational) -> String {
    format!("{}/{}", rational.numerator, rational.denominator)
}

fn output_status_label(snapshot: &Snapshot) -> Div {
    let mut lines = vec![snapshot.output.status.clone()];
    if !snapshot.output.effective_label.is_empty() {
        lines.push(format!(
            "Active output: {}",
            snapshot.output.effective_label
        ));
    }
    let mut block = div().w_full().flex().flex_col();
    for line in lines {
        block = block.child(label(line, LabelVariant::Note, true).w_full());
    }
    block
}

#[allow(clippy::too_many_arguments)]
fn choice_row(
    (id, name): (String, String),
    caption: SharedString,
    detail: Option<SharedString>,
    reason: SharedString,
    show_reason: bool,
    enabled: bool,
    selected: bool,
    focus: FocusHandle,
    activate: Option<Activate>,
) -> Stateful<Div> {
    let caption_color = if enabled {
        brume::FOREGROUND
    } else {
        brume::DISABLED_TEXT
    };
    let detail_color = if enabled {
        brume::SECONDARY
    } else {
        brume::DISABLED_TEXT
    };
    let indicator_color = if !enabled {
        brume::DISABLED_TEXT
    } else if selected {
        brume::ACCENT
    } else {
        brume::DIVIDER
    };
    let mut accessible = caption.to_string();
    if let Some(detail) = &detail
        && !detail.is_empty()
    {
        accessible.push_str(", ");
        accessible.push_str(detail);
    }
    if !reason.is_empty() {
        accessible.push_str(", ");
        accessible.push_str(&reason);
    }
    let mut lines = div()
        .flex_1()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .w_full()
                .text_size(px(brume::FONT_SIZE))
                .text_color(caption_color)
                .child(caption),
        );
    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        lines = lines.child(
            div()
                .w_full()
                .text_size(px(brume::FONT_SIZE))
                .text_color(detail_color)
                .child(detail),
        );
    }
    if show_reason && !reason.is_empty() {
        lines = lines.child(
            div()
                .w_full()
                .text_size(px(brume::FONT_SIZE_SMALL))
                .text_color(detail_color)
                .child(reason),
        );
    }
    let mut row = div()
        .id(id)
        .accessibility_id(name)
        .w_full()
        .min_h(px(brume::CONTROL_HEIGHT))
        .flex()
        .items_center()
        .gap(px(brume::GAP))
        .py(px(brume::GAP_SMALL))
        .px(px(brume::GAP))
        .rounded(px(brume::RADIUS_CONTROL))
        .role(Role::RadioButton)
        .aria_label(accessible)
        .aria_toggled(if selected {
            Toggled::True
        } else {
            Toggled::False
        })
        .tab_stop(enabled)
        .when(enabled, |row| row.track_focus(&focus))
        .child(
            div()
                .w(px(18.))
                .h(px(18.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(indicator_color)
                .when(selected, |indicator| {
                    indicator.child(div().w(px(8.)).h(px(8.)).rounded_full().bg(brume::ACCENT))
                }),
        )
        .child(lines);
    if enabled {
        row = row
            .hover(|style| style.bg(brume::CONTROL_HOVER))
            .active(|style| style.bg(brume::CONTROL_PRESSED));
        if selected {
            row = row.bg(brume::CONTROL);
        }
        if let Some(activate) = activate {
            row = with_activation(row, activate);
        }
    }
    row.focus_visible(|style| style.border_1().border_color(brume::ACCENT))
}

#[allow(clippy::too_many_arguments)]
fn icon_button(
    id: &'static str,
    icon: &'static str,
    label_text: &'static str,
    enabled: bool,
    focus: &FocusHandle,
    regions: &HitRegions,
    activate: Option<Activate>,
) -> Stateful<Div> {
    let tint = if enabled {
        brume::SECONDARY
    } else {
        brume::DISABLED_TEXT
    };
    let mut element = div()
        .id(id)
        .w(px(brume::HIT_TARGET))
        .h(px(brume::HIT_TARGET))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(brume::RADIUS_CONTROL))
        .role(Role::Button)
        .aria_label(label_text)
        .aria_description(label_text)
        .text_size(px(brume::FONT_SIZE))
        .text_color(tint)
        .tab_stop(enabled)
        .when(enabled, |button| button.track_focus(focus))
        .child(glyph(brume::icon(icon), brume::RIBBON_ICON_SIZE, tint));
    if enabled {
        element = element
            .hover(|style| style.bg(brume::CONTROL_HOVER))
            .active(|style| style.bg(brume::CONTROL_PRESSED));
        element = element.child(hit_layer(regions));
        if let Some(activate) = activate {
            element = with_activation(element, activate);
        }
    }
    element.focus_visible(|style| style.border_2().border_color(brume::ACCENT))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LabelVariant {
    Secondary,
    Note,
    Error,
    Title,
}

/// Plain-text Brume label. GPUI text is never interpreted as markup, so device
/// names, diagnostics and catalog text paint directly.
fn label(text: impl Into<SharedString>, variant: LabelVariant, enabled: bool) -> Div {
    let text: SharedString = text.into();
    let (size, weight, color) = match variant {
        LabelVariant::Title => (
            brume::FONT_SIZE_TITLE,
            brume::FONT_WEIGHT_STRONG,
            brume::FOREGROUND,
        ),
        LabelVariant::Secondary => (
            brume::FONT_SIZE,
            brume::FONT_WEIGHT_NORMAL,
            brume::SECONDARY,
        ),
        LabelVariant::Note => (
            brume::FONT_SIZE_SMALL,
            brume::FONT_WEIGHT_NORMAL,
            brume::SECONDARY,
        ),
        LabelVariant::Error => (brume::FONT_SIZE, brume::FONT_WEIGHT_NORMAL, brume::ERROR),
    };
    div()
        .text_size(px(size))
        .font_weight(weight)
        .text_color(if enabled { color } else { brume::DISABLED_TEXT })
        .child(text)
}

/// One bundled SVG glyph painted at its stable asset path, matching the shared
/// control primitive (the pinned GPUI `Svg` element hashes and copies bytes on
/// every construction, so `Window::paint_svg` is used instead).
fn glyph(icon: Option<brume::Icon>, size: f32, tint: gpui::Rgba) -> impl IntoElement {
    div()
        .w(px(size))
        .h(px(size))
        .flex()
        .items_center()
        .justify_center()
        .children(icon.map(|icon| svg_glyph(icon, size, tint)))
}

fn svg_glyph(icon: brume::Icon, size: f32, tint: gpui::Rgba) -> impl IntoElement {
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

/// Wire pointer/keyboard click and the AT-SPI `Click` action to one dispatch,
/// matching the shared control primitive.
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
            gpui::AccessibleAction::Click,
            move |_: Option<&gpui::accesskit::ActionData>, window, cx| {
                accessibility(Activation::Accessibility, window, cx);
            },
        )
}

/// A transparent, full-bleed prepaint listener recording the rendered
/// rectangle, so the companion's X input region names the pixels GPUI actually
/// committed. Rows never record: while a popup is open the input region is the
/// whole panel rectangle.
fn hit_layer(regions: &HitRegions) -> impl IntoElement {
    let regions = regions.clone();
    canvas(move |bounds, _, _| regions.record(bounds), |_, _, _, _| {})
        .absolute()
        .inset_0()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_host_top_chrome_rate_fps_stays_whole_for_integral_rates() {
        assert_eq!(rate_fps("60"), "60 FPS");
        assert_eq!(rate_fps("60000/1000"), "60 FPS");
    }

    #[test]
    fn gpui_host_top_chrome_rate_fps_approximates_genuine_fractions() {
        assert_eq!(rate_fps("60000/1001"), "59.94 FPS");
        assert_eq!(rate_fps("30000/1001"), "29.97 FPS");
        assert_eq!(rate_fps("10/4"), "2.5 FPS");
    }

    #[test]
    fn gpui_host_top_chrome_rate_fps_returns_non_rational_text_verbatim() {
        assert_eq!(rate_fps(""), "");
        assert_eq!(rate_fps("60/0"), "60/0");
        assert_eq!(rate_fps("60/"), "60/");
        assert_eq!(rate_fps("abc"), "abc");
        assert_eq!(rate_fps("1/2/3"), "1/2/3");
        assert_eq!(rate_fps("11529215046068469777/1"), "11529215046068469777/1");
    }

    #[test]
    fn gpui_host_top_chrome_decimal_tokens_reject_malformed_values() {
        assert_eq!(decimal_token("0"), Some(0));
        assert_eq!(decimal_token("42"), Some(42));
        assert_eq!(decimal_token(""), None);
        assert_eq!(decimal_token("0042"), None);
        assert_eq!(decimal_token("-1"), None);
        assert_eq!(decimal_token("abc"), None);
    }

    #[test]
    fn gpui_host_mode_editors_require_positive_u32_components() {
        for integer in ["1", "00060", "4294967295"] {
            assert!(positive_u32(integer), "{integer}");
            assert!(valid_exact_rate(integer), "{integer}");
        }
        for invalid in ["", "0", "+1", "-1", " 1", "1 ", "4294967296", "６０"] {
            assert!(!positive_u32(invalid), "{invalid:?}");
            assert!(!valid_exact_rate(invalid), "{invalid:?}");
        }
        assert!(valid_exact_rate("60000/1001"));
        assert!(valid_exact_rate("4294967295/4294967295"));
        for invalid in [
            "60/0",
            "0/60",
            "60/",
            "/60",
            "60/1/1",
            "59.94",
            "60/4294967296",
        ] {
            assert!(!valid_exact_rate(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn gpui_host_top_chrome_selector_keys_map_to_the_qml_popup_names() {
        assert_eq!(SelectorKind::Source.invoker(), "source");
        assert_eq!(SelectorKind::Output.invoker(), "output");
        assert_eq!(SelectorKind::Size.invoker(), "size");
        assert_eq!(SelectorKind::Rate.invoker(), "rate");
        assert_eq!(
            SelectorKind::from_invoker("gate-options"),
            None,
            "the qualification gate popup stays out of the selector surface"
        );
    }
}
