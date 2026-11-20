//! Capture-format and audio settings bodies for the GPUI frontend.
//!
//! `qml/CaptureSettingsView.qml` and `qml/AudioSettingsView.qml` are the parity
//! contract. This module owns only those two page bodies: the shared Back/
//! title/version header, the settings index, the surrounding popup shell, its
//! placement and its two-second catalog refresh cadence belong to the parent.
//!
//! Both pages apply their complete selections immediately; there is no local
//! draft and no Apply button. Identity rules carried over from the QML views:
//!
//! - A choice row binds the opaque projected key and the revisions *as
//!   rendered*. A valid stale pair still reaches the checked
//!   `RuntimeCoordinator` guard, which refuses it; a malformed revision token
//!   dispatches nothing.
//! - The projected key is authoritative. A key with no matching row stays
//!   visibly selected-but-unavailable and is never remapped onto another row.
//! - Unavailable rows stay listed but disabled, and their reason lives in the
//!   accessible name only (architecture §6) — never an extra paragraph under
//!   the selector.
//! - Audio capture (input) stays distinct from the playback output route and
//!   mute. Mute never changes the selected route.
//!
//! The dropdowns are page-local disclosures, not new native/GPUI windows: the
//! QML `BrumeChoice` popup is a `Popup.Item` bounded to its owner tool, so the
//! opened list grows inside the settings body and the companion keeps its one
//! overlay surface.
//!
//! Host integration surface: [`paint`] renders one page body, [`focus_targets`]
//! feeds the popup Tab chain (including the opened list's enabled rows), and
//! the parent decides when the section is open and arming the shared refresh
//! tick.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use gpui::{
    AccessibleAction, App, ClickEvent, Context, Div, FocusHandle, Hsla, Rgba, Role, SharedString,
    Stateful, Toggled, TransformationMatrix, WeakEntity, canvas, div, prelude::*, px,
};

use crate::ui::projection::{Live, Output, Selection, Snapshot};

use super::{
    HostState, brume,
    chrome::Popup,
    companion_view::CompanionView,
    controls::{Activate, Activation, HitRegions},
    top_chrome::FocusTarget,
};

/// Stable element identities matching the QML `objectName`s.
pub(super) const CAPTURE_REFRESH_ID: &str = "captureRefresh";
pub(super) const CAPTURE_FORMAT_ERROR_ID: &str = "captureFormatError";
pub(super) const CAPTURE_FORMAT_ID: &str = "captureFormatSelector";
pub(super) const CAPTURE_FORMAT_RESOLVED_ID: &str = "captureFormatResolved";
pub(super) const AUDIO_ENABLED_ID: &str = "audioCaptureEnabled";
pub(super) const AUDIO_MISSING_INPUT_ID: &str = "audioMissingInput";
pub(super) const AUDIO_INPUT_ID: &str = "audioInputSelector";
pub(super) const AUDIO_REFRESH_ID: &str = "audioRefresh";
pub(super) const AUDIO_RETAINED_ID: &str = "audioRetained";
pub(super) const AUDIO_CAPTURE_STATUS_ID: &str = "audioCaptureStatus";
pub(super) const AUDIO_OUTPUT_ID: &str = "audioOutputSelector";
pub(super) const AUDIO_OUTPUT_MUTE_ID: &str = "audioOutputMute";
pub(super) const AUDIO_OUTPUT_STATUS_ID: &str = "audioOutputStatus";

const REFRESH_LABEL: &str = "Refresh catalogs";
const FORMAT_UNAVAILABLE: &str = "Format unavailable";
const INPUT_EMPTY: &str = "No input";
const CAPTURE_NOTE: &str = "A previously selected mode that became incompatible stays listed as unavailable until an explicit choice.";
const RETAINED_PREFIX: &str = "Input retained when audio is disabled: ";
const MISSING_INPUT_NOTE: &str = "No input selected: choose an input, then enable audio capture.";
const MUTE_NOTE: &str = "Mute does not replace the selected route; the output stays selected.";

/// The settings section this module renders. The parent owns navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Page {
    Capture,
    Audio,
}

/// Which page dropdown is currently expanded. At most one at a time, and only
/// on the page that owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    CaptureFormat,
    AudioInput,
    AudioOutput,
}

/// One dispatch captured at paint time, with the rendered row key and the
/// revisions shown in that frame.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Intent {
    Refresh,
    SelectFormat {
        revision: String,
        catalog: String,
        row: String,
    },
    SetAudioEnabled {
        revision: String,
        enabled: bool,
    },
    SelectInput {
        revision: String,
        catalog: String,
        row: String,
    },
    SelectOutput {
        row: String,
        revision: u64,
    },
    SetMuted(bool),
}

/// Page state owned by the companion: fixed focus handles plus the per-row
/// nodes (keyed by opaque row key) and the opened-dropdown identity. The open
/// cell is shared with activation closures so a selection closes its own list.
pub(super) struct DeviceSettings {
    open: Rc<Cell<Option<Choice>>>,
    capture_refresh: FocusHandle,
    capture_format: FocusHandle,
    audio_enabled: FocusHandle,
    audio_input: FocusHandle,
    audio_refresh: FocusHandle,
    audio_output: FocusHandle,
    audio_mute: FocusHandle,
    format_rows: RefCell<HashMap<String, FocusHandle>>,
    input_rows: RefCell<HashMap<String, FocusHandle>>,
    output_rows: RefCell<HashMap<String, FocusHandle>>,
}

impl DeviceSettings {
    pub(super) fn new(cx: &mut Context<CompanionView>) -> Self {
        Self {
            open: Rc::default(),
            capture_refresh: cx.focus_handle(),
            capture_format: cx.focus_handle(),
            audio_enabled: cx.focus_handle(),
            audio_input: cx.focus_handle(),
            audio_refresh: cx.focus_handle(),
            audio_output: cx.focus_handle(),
            audio_mute: cx.focus_handle(),
            format_rows: RefCell::default(),
            input_rows: RefCell::default(),
            output_rows: RefCell::default(),
        }
    }

    fn handle_row(
        rows: &RefCell<HashMap<String, FocusHandle>>,
        key: &str,
        cx: &App,
    ) -> FocusHandle {
        rows.borrow_mut()
            .entry(key.to_owned())
            .or_insert_with(|| cx.focus_handle())
            .clone()
    }

    fn retain_rows(rows: &RefCell<HashMap<String, FocusHandle>>, keys: &[String]) {
        rows.borrow_mut().retain(|key, _| keys.contains(key));
    }

    fn is_open(&self, choice: Choice) -> bool {
        self.open.get() == Some(choice)
    }

    /// Collapse any expanded dropdown. The parent calls this when the owning
    /// popup closes or its identity is replaced, where no page repaint would
    /// otherwise reset the disclosure. Page switching is already handled by
    /// [`paint`].
    pub(super) fn close_open_choice(&self) {
        self.open.set(None);
    }
}

/// Lexical decimal token check, mirroring the runtime's `capture_catalog::decimal`
/// contract: `0` or a non-zero-leading digit string, never a float conversion.
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

/// `CaptureSettingsView` editable gate: the selection can be edited and one is
/// actually projected.
fn capture_editable(snapshot: &Snapshot) -> bool {
    snapshot.can_edit_selection && snapshot.selection.is_some()
}

/// `CaptureSettingsView` selected format key: an explicit manual format, else
/// the `auto` policy row.
fn selected_format_key(selection: &Selection) -> &str {
    if selection.format_policy == "manual" {
        &selection.manual_format_key
    } else {
        "auto"
    }
}

/// First row whose opaque key matches `key`.
fn choice_label<'a, I>(key: &str, rows: I) -> Option<&'a str>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    rows.into_iter()
        .find(|(row_key, _)| *row_key == key)
        .map(|(_, label)| label)
}

/// The trigger caption: the projected key's label, else the page's empty text.
/// A missing key is never mapped onto a different row.
fn choice_display<'a>(
    key: &str,
    rows: impl IntoIterator<Item = (&'a str, &'a str)>,
    empty: &'a str,
) -> &'a str {
    if key.is_empty() {
        empty
    } else {
        choice_label(key, rows).unwrap_or(empty)
    }
}

/// `CaptureSettingsView` Requested/Resolved line.
fn format_state_text(selection: &Selection) -> String {
    let requested = if selection.requested_format_label.is_empty() {
        "—"
    } else {
        &selection.requested_format_label
    };
    let resolved = if selection.resolved_format_label.is_empty() {
        "—"
    } else {
        &selection.resolved_format_label
    };
    format!("Requested: {requested}   Resolved: {resolved}")
}

/// QML `visible: !selection.audioEnabled && selection.audioInputKey.length === 0`.
fn missing_input_note(selection: &Selection) -> bool {
    !selection.audio_enabled && selection.audio_input_key.is_empty()
}

/// QML `visible: selection.audioInputLabel.length > 0`.
fn retained_input_note(selection: &Selection) -> Option<String> {
    (!selection.audio_input_label.is_empty())
        .then(|| format!("{RETAINED_PREFIX}{}", selection.audio_input_label))
}

/// QML audio capture status: `Status: <live.audioStatus|—>` plus an optional
/// diagnostic line. Painted as separate text lines.
fn audio_status_lines(live: &Live) -> Vec<String> {
    let status = if live.audio_status.is_empty() {
        "—"
    } else {
        &live.audio_status
    };
    let mut lines = vec![format!("Status: {status}")];
    if !live.audio_diagnostic.is_empty() {
        lines.push(live.audio_diagnostic.clone());
    }
    lines
}

/// QML output status: the route status and, independently, the effective sink.
fn output_status_lines(output: &Output) -> Vec<String> {
    let mut lines = Vec::new();
    if !output.status.is_empty() {
        lines.push(output.status.clone());
    }
    if !output.effective_label.is_empty() {
        lines.push(format!("Active output: {}", output.effective_label));
    }
    lines
}

// ── Dispatch ────────────────────────────────────────────────────────────────

fn accepts_settings(state: &HostState, popup: Popup) -> bool {
    state.chrome.popup() == Some(popup)
        && state.accepts_input()
        && state.view.close_dialog.is_empty()
        && state.overlay_opacity > 0.
}

impl HostState {
    /// Dispatch one rendered intent. The popup identity is re-checked first;
    /// runtime guards then own staleness and availability, and their refusal is
    /// projected by consuming the resulting update.
    fn device_settings_dispatch(&mut self, popup: Popup, intent: &Intent) -> bool {
        if !accepts_settings(self, popup) {
            return false;
        }
        match intent {
            Intent::Refresh => {
                // qml/Chrome.qml `refreshCatalogs` refuses while closing.
                if self
                    .view
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.closing)
                {
                    return false;
                }
                let update = self.runtime.refresh_catalogs();
                self.consume(update);
            }
            Intent::SelectFormat {
                revision,
                catalog,
                row,
            } => {
                if decimal_token(revision).is_none() || decimal_token(catalog).is_none() {
                    return false;
                }
                let update = self.runtime.select_format(revision, catalog, row);
                self.consume(update);
            }
            Intent::SetAudioEnabled { revision, enabled } => {
                if decimal_token(revision).is_none() {
                    return false;
                }
                let update = self.runtime.set_audio_enabled(revision, *enabled);
                self.consume(update);
            }
            Intent::SelectInput {
                revision,
                catalog,
                row,
            } => {
                if decimal_token(revision).is_none() || decimal_token(catalog).is_none() {
                    return false;
                }
                let update = self.runtime.select_audio_input(revision, catalog, row);
                self.consume(update);
            }
            Intent::SelectOutput { row, revision } => {
                // SubmitStatus intents poll and consume so projected refusals
                // render (the top-chrome Output selector pattern).
                let _status = self.runtime.select_output(row, *revision);
                let update = self.runtime.poll();
                self.consume(update);
            }
            Intent::SetMuted(muted) => {
                let _status = self.runtime.set_muted(*muted);
                let update = self.runtime.poll();
                self.consume(update);
            }
        }
        true
    }
}

// ── Activation ──────────────────────────────────────────────────────────────

/// Activation for one runtime intent. `close` names the dropdown that must
/// collapse once its row was used and the trigger that receives focus back.
fn intent_activation(
    host: Rc<RefCell<HostState>>,
    view: WeakEntity<CompanionView>,
    popup: Popup,
    open: Rc<Cell<Option<Choice>>>,
    close: Option<(Choice, FocusHandle)>,
    intent: Intent,
) -> Activate {
    Rc::new(move |_source, window, cx| {
        let dispatched = host.borrow_mut().device_settings_dispatch(popup, &intent);
        if !dispatched {
            return;
        }
        if let Some((choice, trigger)) = &close {
            if open.get() == Some(*choice) {
                open.set(None);
            }
            trigger.focus(window, cx);
        }
        let _ = view.update(cx, |_, cx| cx.notify());
    })
}

/// Toggle one page dropdown. Opening a catalog-backed choice refreshes the
/// catalog exactly like `refreshCatalogOnOpen`.
fn trigger_activation(
    host: Rc<RefCell<HostState>>,
    view: WeakEntity<CompanionView>,
    popup: Popup,
    open: Rc<Cell<Option<Choice>>>,
    choice: Choice,
    refresh_on_open: bool,
) -> Activate {
    Rc::new(move |_source, _window, cx| {
        let mut state = host.borrow_mut();
        if !accepts_settings(&state, popup) {
            return;
        }
        if open.get() == Some(choice) {
            open.set(None);
        } else {
            let closing = state
                .view
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.closing);
            open.set(Some(choice));
            if refresh_on_open && !closing {
                let update = state.runtime.refresh_catalogs();
                state.consume(update);
            }
        }
        drop(state);
        let _ = view.update(cx, |_, cx| cx.notify());
    })
}

/// Build a row intent only when the revisions rendered with the row are
/// well-formed. A valid stale pair still reaches the runtime guard.
fn format_intent(snapshot: &Snapshot, row: &str) -> Option<Intent> {
    (decimal_token(&snapshot.selection_revision).is_some()
        && decimal_token(&snapshot.catalog.revision).is_some())
    .then(|| Intent::SelectFormat {
        revision: snapshot.selection_revision.clone(),
        catalog: snapshot.catalog.revision.clone(),
        row: row.to_owned(),
    })
}

fn input_intent(snapshot: &Snapshot, row: &str) -> Option<Intent> {
    (decimal_token(&snapshot.selection_revision).is_some()
        && decimal_token(&snapshot.catalog.revision).is_some())
    .then(|| Intent::SelectInput {
        revision: snapshot.selection_revision.clone(),
        catalog: snapshot.catalog.revision.clone(),
        row: row.to_owned(),
    })
}

fn output_intent(output: &Output, row: &str) -> Option<Intent> {
    decimal_token(&output.revision).map(|revision| Intent::SelectOutput {
        row: row.to_owned(),
        revision,
    })
}

fn audio_enabled_intent(snapshot: &Snapshot, enabled: bool) -> Option<Intent> {
    decimal_token(&snapshot.selection_revision).map(|_| Intent::SetAudioEnabled {
        revision: snapshot.selection_revision.clone(),
        enabled,
    })
}

// ── Focus targets ───────────────────────────────────────────────────────────

/// The page's contribution to the popup Tab chain, in QML creation order and
/// skipping hidden/disabled controls. Opened lists contribute their enabled
/// rows directly after the trigger.
pub(super) fn focus_targets(
    settings: &DeviceSettings,
    state: &HostState,
    page: Page,
    cx: &App,
) -> Vec<FocusTarget> {
    if !state.accepts_input() || !state.view.close_dialog.is_empty() || state.overlay_opacity <= 0.
    {
        return Vec::new();
    }
    let Some(snapshot) = state.view.snapshot.as_deref() else {
        return Vec::new();
    };
    let closing = snapshot.closing;
    let mut targets = Vec::new();
    match page {
        Page::Capture => {
            if !snapshot.catalog.busy && !closing {
                targets.push(FocusTarget::plain(&settings.capture_refresh));
            }
            if capture_editable(snapshot) {
                targets.push(FocusTarget::plain(&settings.capture_format));
                if settings.is_open(Choice::CaptureFormat) {
                    for row in snapshot
                        .catalog
                        .format_rows
                        .iter()
                        .filter(|row| row.eligible)
                    {
                        targets.push(FocusTarget::plain(&DeviceSettings::handle_row(
                            &settings.format_rows,
                            &row.key,
                            cx,
                        )));
                    }
                }
            }
        }
        Page::Audio => {
            let editable = capture_editable(snapshot);
            if editable {
                targets.push(FocusTarget::plain(&settings.audio_enabled));
                targets.push(FocusTarget::plain(&settings.audio_input));
                if settings.is_open(Choice::AudioInput) {
                    for row in snapshot
                        .catalog
                        .audio_input_rows
                        .iter()
                        .filter(|row| row.eligible)
                    {
                        targets.push(FocusTarget::plain(&DeviceSettings::handle_row(
                            &settings.input_rows,
                            &row.key,
                            cx,
                        )));
                    }
                }
            }
            if !snapshot.catalog.busy && !closing {
                targets.push(FocusTarget::plain(&settings.audio_refresh));
            }
            targets.push(FocusTarget::plain(&settings.audio_output));
            if settings.is_open(Choice::AudioOutput) {
                for row in snapshot.output.rows.iter().filter(|row| row.eligible) {
                    targets.push(FocusTarget::plain(&DeviceSettings::handle_row(
                        &settings.output_rows,
                        &row.key,
                        cx,
                    )));
                }
            }
            targets.push(FocusTarget::plain(&settings.audio_mute));
        }
    }
    targets
}

// ── Painting ────────────────────────────────────────────────────────────────

/// Render the open section's body. The parent owns the header, panel and
/// section choice; this never maps or positions a window.
pub(super) fn paint(
    settings: &DeviceSettings,
    host: &Rc<RefCell<HostState>>,
    page: Page,
    popup: Popup,
    regions: &HitRegions,
    cx: &mut Context<CompanionView>,
) -> Div {
    match page {
        Page::Capture => paint_capture(settings, host, popup, regions, cx),
        Page::Audio => paint_audio(settings, host, popup, regions, cx),
    }
}

fn paint_capture(
    settings: &DeviceSettings,
    host: &Rc<RefCell<HostState>>,
    popup: Popup,
    regions: &HitRegions,
    cx: &mut Context<CompanionView>,
) -> Div {
    // A page switch cannot leave another page's list expanded.
    if matches!(
        settings.open.get(),
        Some(Choice::AudioInput | Choice::AudioOutput)
    ) {
        settings.open.set(None);
    }
    let weak = cx.weak_entity();
    let state = host.borrow();
    let Some(snapshot) = state.view.snapshot.as_deref() else {
        return div();
    };
    let editable = capture_editable(snapshot);
    if !editable && settings.is_open(Choice::CaptureFormat) {
        settings.open.set(None);
    }

    let keys: Vec<String> = snapshot
        .catalog
        .format_rows
        .iter()
        .map(|row| row.key.clone())
        .collect();
    DeviceSettings::retain_rows(&settings.format_rows, &keys);

    let refreshable = !snapshot.catalog.busy && !snapshot.closing;
    let refresh_activate = refreshable.then(|| {
        intent_activation(
            Rc::clone(host),
            weak.clone(),
            popup,
            Rc::clone(&settings.open),
            None,
            Intent::Refresh,
        )
    });

    let mut header = div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(brume::GAP))
        .child(div().flex_1().min_w(px(0.)));
    header = header.child(icon_button(
        CAPTURE_REFRESH_ID,
        "refresh",
        REFRESH_LABEL,
        refreshable,
        &settings.capture_refresh,
        regions,
        refresh_activate,
    ));

    let selected_key = snapshot
        .selection
        .as_ref()
        .map(selected_format_key)
        .unwrap_or("");
    let display = choice_display(
        selected_key,
        snapshot
            .catalog
            .format_rows
            .iter()
            .map(|row| (row.key.as_str(), row.label.as_str())),
        FORMAT_UNAVAILABLE,
    );
    let open = settings.is_open(Choice::CaptureFormat);
    let trigger = trigger_activation(
        Rc::clone(host),
        weak.clone(),
        popup,
        Rc::clone(&settings.open),
        Choice::CaptureFormat,
        true,
    );
    let format_trigger = choice_trigger(
        CAPTURE_FORMAT_ID,
        "Input format",
        display.to_owned(),
        editable,
        open,
        &settings.capture_format,
        regions,
        editable.then_some(trigger),
    );

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(brume::GAP))
        .child(header)
        .when(!snapshot.catalog.error.is_empty(), |body| {
            body.child(
                label(&snapshot.catalog.error, LabelVariant::Error)
                    .id(CAPTURE_FORMAT_ERROR_ID)
                    .w_full(),
            )
        })
        .child(format_trigger)
        .when(open, |body| {
            body.child(choice_list(
                snapshot
                    .catalog
                    .format_rows
                    .iter()
                    .map(|row| OptionModel {
                        key: row.key.clone(),
                        label: row.label.clone(),
                        reason: row.reason.clone(),
                        selected: row.key == selected_key,
                        enabled: editable && row.eligible,
                        intent: format_intent(snapshot, &row.key),
                    })
                    .collect(),
                Choice::CaptureFormat,
                settings,
                host,
                weak.clone(),
                popup,
                &settings.capture_format,
                regions,
                cx,
            ))
        })
        .when_some(snapshot.selection.as_ref(), |body, selection| {
            body.child(
                label(&format_state_text(selection), LabelVariant::Note)
                    .id(CAPTURE_FORMAT_RESOLVED_ID)
                    .w_full(),
            )
        })
        .child(label(CAPTURE_NOTE, LabelVariant::Note).w_full())
}

fn paint_audio(
    settings: &DeviceSettings,
    host: &Rc<RefCell<HostState>>,
    popup: Popup,
    regions: &HitRegions,
    cx: &mut Context<CompanionView>,
) -> Div {
    if settings.is_open(Choice::CaptureFormat) {
        settings.open.set(None);
    }
    let weak = cx.weak_entity();
    let state = host.borrow();
    let Some(snapshot) = state.view.snapshot.as_deref() else {
        return div();
    };
    let editable = capture_editable(snapshot);
    if !editable && settings.is_open(Choice::AudioInput) {
        settings.open.set(None);
    }

    let input_keys: Vec<String> = snapshot
        .catalog
        .audio_input_rows
        .iter()
        .map(|row| row.key.clone())
        .collect();
    DeviceSettings::retain_rows(&settings.input_rows, &input_keys);
    let output_keys: Vec<String> = snapshot
        .output
        .rows
        .iter()
        .map(|row| row.key.clone())
        .collect();
    DeviceSettings::retain_rows(&settings.output_rows, &output_keys);

    let refreshable = !snapshot.catalog.busy && !snapshot.closing;
    let refresh_activate = refreshable.then(|| {
        intent_activation(
            Rc::clone(host),
            weak.clone(),
            popup,
            Rc::clone(&settings.open),
            None,
            Intent::Refresh,
        )
    });

    // Capture enable is bound to the projected selection and the editable gate.
    let enabled_intent = audio_enabled_intent(
        snapshot,
        !snapshot
            .selection
            .as_ref()
            .is_some_and(|selection| selection.audio_enabled),
    );
    let enabled_activate = if editable && snapshot.selection.is_some() {
        enabled_intent.map(|intent| {
            intent_activation(
                Rc::clone(host),
                weak.clone(),
                popup,
                Rc::clone(&settings.open),
                None,
                intent,
            )
        })
    } else {
        None
    };
    let checked = snapshot
        .selection
        .as_ref()
        .is_some_and(|selection| selection.audio_enabled);
    let capture_toggle = toggle(
        AUDIO_ENABLED_ID,
        "Capture audio",
        "Capture audio from the source",
        checked,
        editable,
        &settings.audio_enabled,
        regions,
        enabled_activate,
    );

    let input_selected = snapshot
        .selection
        .as_ref()
        .map(|selection| selection.audio_input_key.as_str())
        .unwrap_or("");
    let input_display = choice_display(
        input_selected,
        snapshot
            .catalog
            .audio_input_rows
            .iter()
            .map(|row| (row.key.as_str(), row.label.as_str())),
        INPUT_EMPTY,
    );
    let input_open = settings.is_open(Choice::AudioInput);
    let input_trigger_activate = trigger_activation(
        Rc::clone(host),
        weak.clone(),
        popup,
        Rc::clone(&settings.open),
        Choice::AudioInput,
        true,
    );
    let input_trigger = choice_trigger(
        AUDIO_INPUT_ID,
        "Captured audio input",
        input_display.to_owned(),
        editable,
        input_open,
        &settings.audio_input,
        regions,
        editable.then_some(input_trigger_activate),
    );

    let capture_row = div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(brume::GAP_SMALL))
        .child(div().flex_1().min_w(px(0.)).child(input_trigger))
        .child(icon_button(
            AUDIO_REFRESH_ID,
            "refresh",
            REFRESH_LABEL,
            refreshable,
            &settings.audio_refresh,
            regions,
            refresh_activate,
        ));

    let selected = snapshot.selection.as_ref();
    let missing = selected.is_some_and(missing_input_note);
    let retained = selected.and_then(retained_input_note);
    let status_lines = audio_status_lines(&snapshot.live);
    let output_status = output_status_lines(&snapshot.output);

    let output_selected = snapshot.output.selected_key.as_str();
    let output_display = choice_display(
        output_selected,
        snapshot
            .output
            .rows
            .iter()
            .map(|row| (row.key.as_str(), row.label.as_str())),
        if snapshot.output.selected_label.is_empty() {
            INPUT_EMPTY
        } else {
            snapshot.output.selected_label.as_str()
        },
    );
    let output_open = settings.is_open(Choice::AudioOutput);
    let output_refresh = trigger_activation(
        Rc::clone(host),
        weak.clone(),
        popup,
        Rc::clone(&settings.open),
        Choice::AudioOutput,
        false,
    );
    let output_trigger = choice_trigger(
        AUDIO_OUTPUT_ID,
        "Playback audio output",
        output_display.to_owned(),
        true,
        output_open,
        &settings.audio_output,
        regions,
        Some(output_refresh),
    );

    let mute_activate = Some(intent_activation(
        Rc::clone(host),
        weak.clone(),
        popup,
        Rc::clone(&settings.open),
        None,
        Intent::SetMuted(!snapshot.live.muted),
    ));
    let mute_label = if snapshot.live.muted {
        "Mute active"
    } else {
        "Mute"
    };
    let mute = toggle(
        AUDIO_OUTPUT_MUTE_ID,
        mute_label,
        "Playback mute",
        snapshot.live.muted,
        true,
        &settings.audio_mute,
        regions,
        mute_activate,
    );

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(brume::GAP))
        .child(label("Captured audio", LabelVariant::Secondary).w_full())
        .child(capture_toggle)
        .when(missing, |body| {
            body.child(
                label(MISSING_INPUT_NOTE, LabelVariant::Note)
                    .id(AUDIO_MISSING_INPUT_ID)
                    .w_full(),
            )
        })
        .child(capture_row)
        .when(input_open, |body| {
            body.child(choice_list(
                snapshot
                    .catalog
                    .audio_input_rows
                    .iter()
                    .map(|row| OptionModel {
                        key: row.key.clone(),
                        label: row.label.clone(),
                        reason: row.reason.clone(),
                        selected: row.key == input_selected,
                        enabled: editable && row.eligible,
                        intent: input_intent(snapshot, &row.key),
                    })
                    .collect(),
                Choice::AudioInput,
                settings,
                host,
                weak.clone(),
                popup,
                &settings.audio_input,
                regions,
                cx,
            ))
        })
        .when_some(retained, |body, text| {
            body.child(
                label(&text, LabelVariant::Note)
                    .id(AUDIO_RETAINED_ID)
                    .w_full(),
            )
        })
        .child(status_block(status_lines, AUDIO_CAPTURE_STATUS_ID))
        .child(
            div()
                .w_full()
                .h(px(brume::DIVIDER_WIDTH))
                .bg(brume::DIVIDER),
        )
        .child(label("Playback output", LabelVariant::Secondary).w_full())
        .child(output_trigger)
        .when(output_open, |body| {
            body.child(choice_list(
                snapshot
                    .output
                    .rows
                    .iter()
                    .map(|row| OptionModel {
                        key: row.key.clone(),
                        label: row.label.clone(),
                        reason: row.reason.clone(),
                        selected: row.key == output_selected,
                        enabled: row.eligible,
                        intent: output_intent(&snapshot.output, &row.key),
                    })
                    .collect(),
                Choice::AudioOutput,
                settings,
                host,
                weak.clone(),
                popup,
                &settings.audio_output,
                regions,
                cx,
            ))
        })
        .child(mute)
        .when(!output_status.is_empty(), |body| {
            body.child(status_block(output_status, AUDIO_OUTPUT_STATUS_ID))
        })
        .child(label(MUTE_NOTE, LabelVariant::Note).w_full())
}

/// One dropdown option rendered from the frame that painted it.
struct OptionModel {
    key: String,
    label: String,
    reason: String,
    selected: bool,
    enabled: bool,
    intent: Option<Intent>,
}

/// The opened list of one dropdown. Rows keep their rendered key identity and
/// a selection collapses the list back to its trigger.
#[allow(clippy::too_many_arguments)]
fn choice_list(
    rows: Vec<OptionModel>,
    choice: Choice,
    settings: &DeviceSettings,
    host: &Rc<RefCell<HostState>>,
    view: WeakEntity<CompanionView>,
    popup: Popup,
    trigger: &FocusHandle,
    regions: &HitRegions,
    cx: &App,
) -> Div {
    let mut list = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(brume::GAP_SMALL))
        .p(px(1.0))
        .rounded(px(brume::RADIUS_SURFACE))
        .border_1()
        .border_color(brume::DIVIDER)
        .bg(brume::MENU_SURFACE);
    for row in rows {
        let OptionModel {
            key,
            label: caption,
            reason,
            selected,
            enabled,
            intent,
        } = row;
        let name = choice_row_id(choice, &key);
        // A press against a previous revision must not release into the
        // replacement listener; the focus and accessibility ID stay stable.
        // The rendered revisions are already part of the intent, so no
        // cross-module popup-epoch access is needed.
        let id = match intent.as_ref() {
            Some(
                Intent::SelectFormat {
                    revision, catalog, ..
                }
                | Intent::SelectInput {
                    revision, catalog, ..
                },
            ) => format!("{revision}:{catalog}:{name}"),
            Some(Intent::SelectOutput { revision, .. }) => format!("{revision}:{name}"),
            _ => name.clone(),
        };
        let activate = intent.filter(|_| enabled).map(|intent| {
            intent_activation(
                Rc::clone(host),
                view.clone(),
                popup,
                Rc::clone(&settings.open),
                Some((choice, trigger.clone())),
                intent,
            )
        });
        let focus = DeviceSettings::handle_row(
            match choice {
                Choice::CaptureFormat => &settings.format_rows,
                Choice::AudioInput => &settings.input_rows,
                Choice::AudioOutput => &settings.output_rows,
            },
            &key,
            cx,
        );
        list = list.child(choice_option(
            (id, name),
            caption,
            reason,
            selected,
            enabled,
            &focus,
            regions,
            activate,
        ));
    }
    list
}

fn choice_row_id(choice: Choice, key: &str) -> String {
    match choice {
        Choice::CaptureFormat => format!("{CAPTURE_FORMAT_ID}_row_{key}"),
        Choice::AudioInput => format!("{AUDIO_INPUT_ID}_row_{key}"),
        Choice::AudioOutput => format!("{AUDIO_OUTPUT_ID}_row_{key}"),
    }
}

// ── Controls ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LabelVariant {
    Secondary,
    Note,
    Error,
}

/// Plain-text Brume label. GPUI text is never interpreted as markup, so device
/// names, reasons and diagnostics paint directly.
fn label(text: &str, variant: LabelVariant) -> Div {
    let (size, color) = match variant {
        LabelVariant::Secondary => (brume::FONT_SIZE, brume::SECONDARY),
        LabelVariant::Note => (brume::FONT_SIZE_SMALL, brume::SECONDARY),
        LabelVariant::Error => (brume::FONT_SIZE, brume::ERROR),
    };
    div()
        .text_size(px(size))
        .font_weight(brume::FONT_WEIGHT_NORMAL)
        .text_color(color)
        .child(SharedString::from(text.to_owned()))
}

/// Multi-line note block with one stable accessible identity.
fn status_block(lines: Vec<String>, id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .accessibility_id(id)
        .role(Role::Label)
        .aria_label(lines.join("\n"))
        .w_full()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .children(lines.into_iter().map(|line| {
            div()
                .w_full()
                .text_size(px(brume::FONT_SIZE_SMALL))
                .text_color(brume::SECONDARY)
                .child(SharedString::from(line))
        }))
}

/// Brume icon button: 48 px hit target, compact glyph, tooltip-equivalent
/// accessible name and description.
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
        .accessibility_id(id)
        .w(px(brume::HIT_TARGET))
        .h(px(brume::HIT_TARGET))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(brume::RADIUS_CONTROL))
        .role(Role::Button)
        .aria_label(label_text)
        .aria_description(label_text)
        .tab_stop(enabled)
        .when(enabled, |button| button.track_focus(focus))
        .child(glyph(brume::icon(icon), brume::RIBBON_ICON_SIZE, tint))
        .focus_visible(|style| style.border_2().border_color(brume::ACCENT));
    if enabled {
        element = element
            .cursor_pointer()
            .hover(|style| style.bg(brume::CONTROL_HOVER))
            .active(|style| style.bg(brume::CONTROL_PRESSED))
            .child(hit_layer(regions));
        if let Some(activate) = activate {
            element = with_activation(element, activate);
        }
    }
    element
}

/// Collapsed dropdown trigger: the projected choice label, a chevron and the
/// same 36 px control geometry as `BrumeChoice`.
#[allow(clippy::too_many_arguments)]
fn choice_trigger(
    id: &'static str,
    accessible: &'static str,
    display: String,
    enabled: bool,
    open: bool,
    focus: &FocusHandle,
    regions: &HitRegions,
    activate: Option<Activate>,
) -> Stateful<Div> {
    let tint = if enabled {
        brume::FOREGROUND
    } else {
        brume::DISABLED_TEXT
    };
    let indicator = if enabled {
        brume::SECONDARY
    } else {
        brume::DISABLED_TEXT
    };
    let mut element = div()
        .id(id)
        .accessibility_id(id)
        .w_full()
        .h(px(brume::CONTROL_HEIGHT))
        .flex()
        .items_center()
        .gap(px(brume::GAP_SMALL))
        .px(px(brume::GAP))
        .rounded(px(brume::RADIUS_CONTROL))
        .border_1()
        .border_color(brume::DIVIDER)
        .bg(if enabled {
            brume::CONTROL
        } else {
            brume::DISABLED_FILL
        })
        .role(Role::ComboBox)
        .aria_label(accessible)
        .aria_expanded(open)
        .tab_stop(enabled)
        .when(enabled, |trigger| trigger.track_focus(focus))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_size(px(brume::FONT_SIZE))
                .text_color(tint)
                .whitespace_nowrap()
                .text_ellipsis()
                .child(SharedString::from(display)),
        )
        .child(glyph(
            brume::icon("chevron-down"),
            brume::ICON_SIZE,
            indicator,
        ))
        .focus_visible(|style| style.border_2().border_color(brume::ACCENT));
    if enabled {
        element = element
            .cursor_pointer()
            .hover(|style| style.bg(brume::CONTROL_HOVER))
            .active(|style| style.bg(brume::CONTROL_PRESSED))
            .child(hit_layer(regions));
        if let Some(activate) = activate {
            element = with_activation(element, activate);
        }
    }
    element
}

/// One dropdown option. Unavailable rows stay listed but disabled; their
/// reason is carried by the accessible name, not a paragraph.
#[allow(clippy::too_many_arguments)]
fn choice_option(
    (id, name): (String, String),
    caption: String,
    reason: String,
    selected: bool,
    enabled: bool,
    focus: &FocusHandle,
    regions: &HitRegions,
    activate: Option<Activate>,
) -> Stateful<Div> {
    let caption_color = if enabled {
        brume::FOREGROUND
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
    let accessible = if enabled || reason.is_empty() {
        caption.clone()
    } else {
        format!("{caption}, {reason}")
    };
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
        .when(enabled, |row| row.track_focus(focus))
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
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_size(px(brume::FONT_SIZE))
                .text_color(caption_color)
                .whitespace_nowrap()
                .text_ellipsis()
                .child(SharedString::from(caption)),
        );
    if enabled {
        row = row
            .cursor_pointer()
            .hover(|style| style.bg(brume::CONTROL_HOVER))
            .active(|style| style.bg(brume::CONTROL_PRESSED))
            .when(selected, |row| row.bg(brume::CONTROL))
            .child(hit_layer(regions));
        if let Some(activate) = activate {
            row = with_activation(row, activate);
        }
    }
    row.focus_visible(|style| style.border_1().border_color(brume::ACCENT))
}

/// Brume checkbox: visible indicator, sentence-case label and a real focus
/// ring. A disabled toggle is genuine off and has no tab stop or hit region.
#[allow(clippy::too_many_arguments)]
fn toggle(
    id: &'static str,
    text: &'static str,
    accessible: &'static str,
    checked: bool,
    enabled: bool,
    focus: &FocusHandle,
    regions: &HitRegions,
    activate: Option<Activate>,
) -> Stateful<Div> {
    let tint = if enabled {
        brume::FOREGROUND
    } else {
        brume::DISABLED_TEXT
    };
    let fill = if !enabled {
        brume::DISABLED_FILL
    } else if checked {
        brume::ACCENT
    } else {
        brume::TRACK
    };
    let mut element = div()
        .id(id)
        .accessibility_id(id)
        .w_full()
        .min_h(px(brume::CONTROL_HEIGHT))
        .flex()
        .items_center()
        .gap(px(brume::GAP))
        .role(Role::CheckBox)
        .aria_label(accessible)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .tab_stop(enabled)
        .when(enabled, |element| element.track_focus(focus))
        .child(
            div()
                .w(px(20.))
                .h(px(20.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .border_1()
                .border_color(if !enabled {
                    brume::DIVIDER
                } else if checked {
                    brume::ACCENT
                } else {
                    brume::DIVIDER
                })
                .bg(fill)
                .when(checked, |indicator| {
                    indicator.child(glyph(brume::icon("check"), 14.0, brume::MATTE))
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(px(brume::FONT_SIZE))
                .text_color(tint)
                .child(text),
        )
        .focus_visible(|style| style.border_1().border_color(brume::ACCENT));
    if enabled {
        element = element
            .cursor_pointer()
            .hover(|style| style.bg(brume::CONTROL_HOVER))
            .active(|style| style.bg(brume::CONTROL_PRESSED))
            .child(hit_layer(regions));
        if let Some(activate) = activate {
            element = with_activation(element, activate);
        }
    }
    element
}

/// Brume hover/press feedback for an enabled control.
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

/// One bundled SVG glyph painted at its stable asset path. GPUI's own `Svg`
/// element hashes and copies the bytes per construction, so the shared
/// primitive is not used here.
fn glyph(icon: Option<brume::Icon>, size: f32, tint: Rgba) -> impl IntoElement {
    div()
        .w(px(size))
        .h(px(size))
        .flex()
        .items_center()
        .justify_center()
        .children(icon.map(move |icon| svg_glyph(icon, size, tint)))
}

fn svg_glyph(icon: brume::Icon, size: f32, tint: Rgba) -> impl IntoElement {
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

#[cfg(test)]
mod tests {
    use super::{
        audio_status_lines, capture_editable, choice_display, decimal_token, format_state_text,
        input_intent, missing_input_note, output_intent, output_status_lines, retained_input_note,
        selected_format_key,
    };
    use crate::ui::projection::{
        AudioInputRow, Catalog, Diagnostics, FormatRow, Live, Output, OutputRow, Selection,
        SessionKind, Snapshot,
    };

    fn selection() -> Selection {
        Selection {
            source_key: "src".into(),
            source_label: "Camera".into(),
            source_available: true,
            size: None,
            rate: None,
            format_policy: "auto",
            manual_format_key: String::new(),
            requested_format_label: String::new(),
            resolved_format_label: String::new(),
            audio_enabled: false,
            audio_input_key: String::new(),
            audio_input_label: String::new(),
            renderer: Default::default(),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            schema: 2,
            session_kind: SessionKind::Capture,
            state_token: "state:1".into(),
            generation: "1".into(),
            product_phase: "Active".into(),
            gate_phase: "Ready".into(),
            paused: false,
            opening: false,
            closing: false,
            can_edit_selection: true,
            can_restart: true,
            can_reconnect: false,
            can_toggle_pause: true,
            can_set_gain: true,
            selection_revision: "3".into(),
            selection: Some(selection()),
            applied: None,
            last_valid: None,
            live: Live::default(),
            stream_stats: None,
            catalog: Catalog {
                revision: "2".into(),
                ..Default::default()
            },
            renderer: Default::default(),
            output: Output::default(),
            recovery: Default::default(),
            settings: Default::default(),
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

    #[test]
    fn decimal_tokens_reject_malformed_revisions() {
        assert_eq!(decimal_token("0"), Some(0));
        assert_eq!(decimal_token("42"), Some(42));
        assert_eq!(decimal_token(""), None);
        assert_eq!(decimal_token("00"), None);
        assert_eq!(decimal_token("01"), None);
        assert_eq!(decimal_token("-1"), None);
        assert_eq!(decimal_token("1.0"), None);
        assert_eq!(decimal_token(" 1"), None);
    }

    #[test]
    fn format_selection_uses_manual_key_only_for_manual_policy() {
        let mut value = selection();
        assert_eq!(selected_format_key(&value), "auto");
        value.format_policy = "manual";
        value.manual_format_key = "format:MJPG".into();
        assert_eq!(selected_format_key(&value), "format:MJPG");
    }

    #[test]
    fn choice_display_never_remaps_a_missing_key() {
        let rows = [("auto", "Auto"), ("format:MJPG", "MJPG")];
        assert_eq!(
            choice_display("format:MJPG", rows.iter().copied(), "unavailable"),
            "MJPG"
        );
        assert_eq!(
            choice_display("format:gone", rows.iter().copied(), "unavailable"),
            "unavailable"
        );
        assert_eq!(
            choice_display("", rows.iter().copied(), "No input"),
            "No input"
        );
    }

    #[test]
    fn format_state_uses_em_dash_for_empty_labels() {
        let mut value = selection();
        assert_eq!(format_state_text(&value), "Requested: —   Resolved: —");
        value.requested_format_label = "MJPG".into();
        value.resolved_format_label = "MJPG".into();
        assert_eq!(
            format_state_text(&value),
            "Requested: MJPG   Resolved: MJPG"
        );
    }

    #[test]
    fn capture_editable_requires_a_projected_selection() {
        let mut value = snapshot();
        assert!(capture_editable(&value));
        value.selection = None;
        assert!(!capture_editable(&value));
        value.selection = Some(selection());
        value.can_edit_selection = false;
        assert!(!capture_editable(&value));
    }

    #[test]
    fn audio_notes_follow_the_qml_visibility_rules() {
        let mut value = selection();
        assert!(missing_input_note(&value));
        value.audio_enabled = true;
        assert!(!missing_input_note(&value));
        value.audio_enabled = false;
        value.audio_input_key = "audio:1".into();
        assert!(!missing_input_note(&value));

        assert_eq!(retained_input_note(&value), None);
        value.audio_input_label = "Microphone".into();
        assert_eq!(
            retained_input_note(&value).as_deref(),
            Some("Input retained when audio is disabled: Microphone")
        );
    }

    #[test]
    fn audio_status_keeps_the_diagnostic_independent() {
        let mut live = Live::default();
        assert_eq!(audio_status_lines(&live), vec!["Status: —"]);
        live.audio_status = "Live".into();
        live.audio_diagnostic = "loopback missing".into();
        assert_eq!(
            audio_status_lines(&live),
            vec!["Status: Live", "loopback missing"]
        );
    }

    #[test]
    fn output_status_combines_route_and_effective_sink() {
        let mut output = Output::default();
        assert!(output_status_lines(&output).is_empty());
        output.status = "Requested output unavailable".into();
        output.effective_label = "Edifier M60".into();
        assert_eq!(
            output_status_lines(&output),
            vec!["Requested output unavailable", "Active output: Edifier M60"]
        );
    }

    #[test]
    fn row_intents_bind_rendered_revisions_and_dispatch_nothing_when_malformed() {
        let value = snapshot();
        assert!(format_intent_for(&value).is_some());
        assert!(input_intent(&value, "audio:1").is_some());
        let mut stale = snapshot();
        stale.catalog.revision = "not-a-revision".into();
        assert!(format_intent_for(&stale).is_none());
        assert!(input_intent(&stale, "audio:1").is_none());
        stale.catalog.revision = "4".into();
        stale.selection_revision = "x".into();
        assert!(format_intent_for(&stale).is_none());
        assert!(input_intent(&stale, "audio:1").is_none());

        let mut output = Output::default();
        assert!(output_intent(&output, "out:1").is_none());
        output.revision = "7".into();
        assert!(output_intent(&output, "out:1").is_some());
    }

    fn format_intent_for(snapshot: &Snapshot) -> Option<super::Intent> {
        super::format_intent(snapshot, "format:MJPG")
    }

    #[test]
    fn catalog_rows_keep_their_reasons_and_order() {
        let catalog = Catalog {
            revision: "2".into(),
            format_rows: vec![
                FormatRow {
                    key: "auto".into(),
                    label: "Auto".into(),
                    fourcc: String::new(),
                    eligible: true,
                    reason: String::new(),
                },
                FormatRow {
                    key: "format:RGB".into(),
                    label: "RGB".into(),
                    fourcc: "RGB3".into(),
                    eligible: false,
                    reason: "not qualified".into(),
                },
            ],
            audio_input_rows: vec![AudioInputRow {
                key: "audio:1".into(),
                label: "Microphone".into(),
                eligible: false,
                reason: "busy".into(),
            }],
            ..Default::default()
        };
        assert_eq!(catalog.format_rows[1].key, "format:RGB");
        assert_eq!(catalog.format_rows[1].reason, "not qualified");
        assert_eq!(catalog.audio_input_rows[0].reason, "busy");
        let output = Output {
            rows: vec![OutputRow {
                key: "out:1".into(),
                label: "Speakers".into(),
                name: "Speakers".into(),
                eligible: false,
                reason: "missing".into(),
            }],
            ..Default::default()
        };
        assert!(!output.rows[0].eligible);
        assert_eq!(output.rows[0].reason, "missing");
    }
}
