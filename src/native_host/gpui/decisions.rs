//! Mandatory decision panels for the GPUI frontend.
//!
//! `qml/ResetSettingsView.qml` and `qml/SaveFailureView.qml` are the parity
//! contract. They are rendered in the companion's `save`/`reset` popup slots,
//! never dismissed by Escape or an outside press, and own no lifecycle of
//! their own: the runtime keeps the pending token and the owner barrier.
//!
//! Identity precedence is the QML `Chrome.mandatoryPopup` rule:
//!
//! 1. `settings.closeDialog == "save"` → [`Identity::SaveFailure`]
//! 2. `settings.closeDialog == "warning"` → [`Identity::DurabilityWarning`]
//! 3. a nonzero projected `settings.resetToken` → [`Identity::Reset`]
//!
//! A missing, zero, malformed or non-canonical reset token is not a decision,
//! and an unknown `closeDialog` value never falls back to reset. Buttons carry
//! a typed [`Intent`] captured from the rendered identity, so the parent can
//! re-check [`Identity::accepts`] before dispatching through
//! `RuntimeCoordinator`; this module never dispatches, maps a window or
//! focuses one itself.
//!
//! Host integration surface: [`DecisionFocus::new`] creates the two
//! caller-owned focus handles, [`DecisionFocus::initial`] and
//! [`DecisionFocus::boundary`] reproduce the QML `focusBoundary`, and
//! [`paint`] renders one panel for the identity derived from the current
//! projection.

use std::{num::NonZeroU64, rc::Rc};

use gpui::{App, Div, FocusHandle, Role, ScrollHandle, SharedString, Window, div, prelude::*, px};

use crate::ui::projection::Settings;

use super::{
    brume,
    controls::{self, Activate, Activation, HitRegions, LabelVariant},
};

/// Popup invoker keys used by `qml/Chrome.qml` for the two mandatory surfaces.
#[allow(
    dead_code,
    reason = "integration surface for the parent's popup registration"
)]
pub(super) const SAVE_POPUP: &str = "save";
#[allow(
    dead_code,
    reason = "integration surface for the parent's popup registration"
)]
pub(super) const RESET_POPUP: &str = "reset";

/// `Brume.settingsWidth`: the shared settings baseline these panels inherit.
#[allow(
    dead_code,
    reason = "integration surface for the parent's panel sizing"
)]
pub(super) const WIDTH: f32 = brume::SETTINGS_WIDTH;

/// Stable element identities. The button ids are the QML `objectName`s; the
/// remaining ids keep the same accessible nodes across paints.
pub(super) const RESET_ID: &str = "resetSettingsPanel";
pub(super) const RESET_TITLE_ID: &str = "resetSettingsTitle";
pub(super) const RESET_PATH_ID: &str = "resetSettingsPath";
pub(super) const RESET_CANCEL_ID: &str = "resetCancel";
pub(super) const RESET_CONFIRM_ID: &str = "resetConfirm";
pub(super) const SAVE_ID: &str = "saveFailurePanel";
pub(super) const SAVE_TITLE_ID: &str = "saveFailureTitle";
pub(super) const SAVE_STATUS_ID: &str = "saveFailureStatus";
pub(super) const SAVE_DIAGNOSTIC_ID: &str = "saveFailureDiagnostic";
pub(super) const SAVE_RETRY_ID: &str = "saveFailureRetry";
pub(super) const SAVE_CLOSE_ID: &str = "saveFailureClose";

const RESET_TITLE: &str = "Replace the settings file?";
const RESET_MESSAGE: &str = "The original file will be replaced and its content lost:";
const RESET_CONSEQUENCE: &str = "Every source profile and the last saved source will be erased. Current global preferences are kept; the running capture is neither stopped nor changed.";
const SAVE_FAILED_TITLE: &str = "Save failed";
const WARNING_TITLE: &str = "Durability not confirmed";
const SAVE_RELEASE: &str = "Media release is complete; no media owner will reopen.";

/// The mandatory decision currently projected by the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Identity {
    /// The displayed nonzero token must be echoed to `decide_reset`.
    Reset(NonZeroU64),
    /// `closeDialog == "save"`: retry or close without saving.
    SaveFailure,
    /// `closeDialog == "warning"`: durability was not confirmed.
    DurabilityWarning,
}

impl Identity {
    /// Resolve the identity with `qml/Chrome.qml` precedence. `close_dialog`
    /// wins over a pending reset token, and any unknown dialog value refuses
    /// to invent a panel.
    pub(super) fn resolve(close_dialog: &str, settings: Option<&Settings>) -> Option<Self> {
        match close_dialog {
            "save" => Some(Self::SaveFailure),
            "warning" => Some(Self::DurabilityWarning),
            "" => settings
                .and_then(|settings| canonical_u64(&settings.reset_token))
                .and_then(NonZeroU64::new)
                .map(Self::Reset),
            _ => None,
        }
    }

    /// The companion popup slot this identity occupies.
    #[allow(
        dead_code,
        reason = "integration surface for the parent's popup registration"
    )]
    pub(super) const fn popup(self) -> &'static str {
        match self {
            Self::Reset(_) => RESET_POPUP,
            Self::SaveFailure | Self::DurabilityWarning => SAVE_POPUP,
        }
    }

    /// Whether the rendered identity still authorizes one captured intent.
    /// The parent checks this before handing the intent to the runtime.
    pub(super) fn accepts(self, intent: Intent) -> bool {
        match (self, intent) {
            (Self::Reset(token), Intent::DecideReset { token: shown, .. }) => token == shown,
            (Self::SaveFailure, Intent::RetrySave | Intent::CloseWithoutSave) => true,
            (Self::DurabilityWarning, Intent::CloseWithoutSave) => true,
            _ => false,
        }
    }
}

/// One dispatch a panel button rendered. Every value is captured at paint
/// time; nothing is looked up again in the activation handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "the parent reads the captured confirmation/token when dispatching"
)]
pub(super) enum Intent {
    DecideReset { token: NonZeroU64, confirmed: bool },
    RetrySave,
    CloseWithoutSave,
}

/// The action a rendered button owns. The parent supplies the dispatch and
/// re-checks the current projection/rendered identity before acting.
pub(super) type Dispatch = Rc<dyn Fn(Intent, Activation, &mut Window, &mut App)>;

/// The projected facts the two panels read.
#[derive(Clone, Copy)]
pub(super) struct DecisionView<'a> {
    /// `UiUpdate.close_dialog` / the retained `ViewState::close_dialog`.
    pub(super) close_dialog: &'a str,
    /// Retained `Snapshot::settings`; `None` means no usable projection.
    pub(super) settings: Option<&'a Settings>,
    /// `UiUpdate.settings_status` (`ViewState::settings_status`).
    pub(super) status: &'a str,
    /// `UiUpdate.diagnostic`, as `SaveFailureView.qml` reads it.
    pub(super) diagnostic: &'a str,
}

impl DecisionView<'_> {
    pub(super) fn identity(&self) -> Option<Identity> {
        Identity::resolve(self.close_dialog, self.settings)
    }
}

/// Caller-owned focus handles for the two button positions.
///
/// `primary` is Cancel (reset) or Retry save (save failure); `secondary` is
/// Confirm (reset) or Close (save failure / durability warning).
#[derive(Clone)]
pub(super) struct DecisionFocus {
    primary: FocusHandle,
    secondary: FocusHandle,
}

impl DecisionFocus {
    pub(super) fn new(cx: &App) -> Self {
        Self {
            primary: cx.focus_handle(),
            secondary: cx.focus_handle(),
        }
    }

    pub(super) fn primary(&self) -> &FocusHandle {
        &self.primary
    }

    pub(super) fn secondary(&self) -> &FocusHandle {
        &self.secondary
    }

    /// The first focus target on open, matching `ResetSettingsView` and
    /// `SaveFailureView` `focusBoundary(false)`. The durability warning has a
    /// single Close button.
    #[allow(
        dead_code,
        reason = "integration surface for the parent's focus restoration"
    )]
    pub(super) fn initial(&self, identity: Identity) -> &FocusHandle {
        match identity {
            Identity::SaveFailure | Identity::Reset(_) => &self.primary,
            Identity::DurabilityWarning => &self.secondary,
        }
    }

    /// The focus target for a Tab/Backtab boundary press: forward keeps the
    /// QML first button, backward always lands on the last button.
    #[allow(
        dead_code,
        reason = "integration surface for the parent's focus restoration"
    )]
    pub(super) fn boundary(&self, identity: Identity, backward: bool) -> &FocusHandle {
        if backward {
            &self.secondary
        } else {
            self.initial(identity)
        }
    }
}

/// Render one mandatory panel. `None` means the projection carries no
/// decision, so the parent must not map a mandatory surface.
///
/// The caller places and scrolls the returned element inside the companion
/// popup; `scroll` only binds the QML body's own viewport when supplied.
pub(super) fn paint(
    view: DecisionView<'_>,
    focus: &DecisionFocus,
    regions: &HitRegions,
    scroll: Option<&ScrollHandle>,
    dispatch: &Dispatch,
) -> Option<gpui::Stateful<Div>> {
    let identity = view.identity()?;
    match identity {
        Identity::Reset(token) => Some(reset_panel(view, token, focus, regions, scroll, dispatch)),
        Identity::SaveFailure | Identity::DurabilityWarning => {
            Some(save_panel(view, identity, focus, regions, scroll, dispatch))
        }
    }
}

fn reset_panel(
    view: DecisionView<'_>,
    token: NonZeroU64,
    focus: &DecisionFocus,
    regions: &HitRegions,
    scroll: Option<&ScrollHandle>,
    dispatch: &Dispatch,
) -> gpui::Stateful<Div> {
    let path = view.settings.map_or("", |settings| settings.path.as_str());
    let cancel = controls::button(
        RESET_CANCEL_ID,
        "Cancel",
        None,
        true,
        regions,
        Some(activation(
            dispatch,
            Intent::DecideReset {
                token,
                confirmed: false,
            },
        )),
    )
    .track_focus(focus.primary())
    .w_full();
    let confirm = controls::button(
        RESET_CONFIRM_ID,
        "Replace the original file",
        None,
        true,
        regions,
        Some(activation(
            dispatch,
            Intent::DecideReset {
                token,
                confirmed: true,
            },
        )),
    )
    .track_focus(focus.secondary())
    .w_full();

    let message = div()
        .flex()
        .flex_col()
        .gap(px(brume::GAP_SMALL))
        .w_full()
        .child(body(RESET_MESSAGE).w_full())
        .child(note(path).id(RESET_PATH_ID).role(Role::Label).w_full())
        .child(body(RESET_CONSEQUENCE).w_full());

    panel(
        RESET_ID,
        RESET_TITLE_ID,
        RESET_TITLE,
        scrollable(message, scroll),
        vec![cancel, confirm],
    )
}

fn save_panel(
    view: DecisionView<'_>,
    identity: Identity,
    focus: &DecisionFocus,
    regions: &HitRegions,
    scroll: Option<&ScrollHandle>,
    dispatch: &Dispatch,
) -> gpui::Stateful<Div> {
    let warning = identity == Identity::DurabilityWarning;
    let title = if warning {
        WARNING_TITLE
    } else {
        SAVE_FAILED_TITLE
    };

    let mut message = div()
        .flex()
        .flex_col()
        .gap(px(brume::GAP_SMALL))
        .w_full()
        .child(body(SAVE_RELEASE).w_full());
    if !view.status.is_empty() {
        message = message.child(
            note(view.status)
                .id(SAVE_STATUS_ID)
                .accessibility_id(SAVE_STATUS_ID)
                .role(Role::Label)
                .w_full(),
        );
    }
    if !view.diagnostic.is_empty() {
        message = message.child(
            note(view.diagnostic)
                .id(SAVE_DIAGNOSTIC_ID)
                .accessibility_id(SAVE_DIAGNOSTIC_ID)
                .role(Role::Label)
                .w_full(),
        );
    }

    let mut buttons = Vec::with_capacity(2);
    if !warning {
        buttons.push(
            controls::button(
                SAVE_RETRY_ID,
                "Retry save",
                None,
                true,
                regions,
                Some(activation(dispatch, Intent::RetrySave)),
            )
            .track_focus(focus.primary())
            .w_full(),
        );
    }
    buttons.push(
        controls::button(
            SAVE_CLOSE_ID,
            if warning {
                "Close without confirmed durability"
            } else {
                "Close without saving"
            },
            None,
            true,
            regions,
            Some(activation(dispatch, Intent::CloseWithoutSave)),
        )
        .track_focus(focus.secondary())
        .w_full(),
    );

    panel(
        SAVE_ID,
        SAVE_TITLE_ID,
        title,
        scrollable(message, scroll),
        buttons,
    )
}

/// QML `ScrollView`: cap the body at the shared list height and scroll it when
/// the parent owns the handle; without one the body clips instead of growing
/// past the popup.
fn scrollable(message: Div, scroll: Option<&ScrollHandle>) -> gpui::Stateful<Div> {
    let message = message
        .id("decision-message")
        .max_h(px(brume::POPUP_MAX_LIST_HEIGHT));
    if let Some(handle) = scroll {
        message.overflow_y_scroll().track_scroll(handle)
    } else {
        message.overflow_hidden()
    }
}

/// Shared title/scroll-body/button layout for the two panels.
fn panel(
    root_id: &'static str,
    title_id: &'static str,
    title: &'static str,
    message: gpui::Stateful<Div>,
    buttons: Vec<gpui::Stateful<Div>>,
) -> gpui::Stateful<Div> {
    let mut root = div()
        .id(root_id)
        .accessibility_id(root_id)
        .role(Role::Group)
        .aria_label(title)
        .flex()
        .flex_col()
        .gap(px(brume::GAP))
        .w_full()
        .child(
            controls::label(title, LabelVariant::Title)
                .id(title_id)
                .accessibility_id(title_id)
                .role(Role::Label)
                .w_full(),
        )
        .child(message);
    for button in buttons {
        root = root.child(button);
    }
    root
}

/// Brume body label (13 px, primary colour).
fn body(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(brume::FONT_SIZE))
        .font_weight(brume::FONT_WEIGHT_NORMAL)
        .text_color(brume::FOREGROUND)
        .child(text.into())
}

/// Brume note label (11 px, secondary colour) for paths and diagnostics.
fn note(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(brume::FONT_SIZE_SMALL))
        .font_weight(brume::FONT_WEIGHT_NORMAL)
        .text_color(brume::SECONDARY)
        .child(text.into())
}

/// Bind one captured intent to a shared control activation.
fn activation(dispatch: &Dispatch, intent: Intent) -> Activate {
    let dispatch = Rc::clone(dispatch);
    Rc::new(move |source, window, cx| dispatch(intent, source, window, cx))
}

/// Canonical unsigned decimal token, matching `Projection.u64`: no sign, no
/// whitespace, no leading zero except `0` itself, and u64-bounded.
fn canonical_u64(value: &str) -> Option<u64> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{Identity, Intent, canonical_u64};
    use crate::ui::projection::Settings;
    use std::num::NonZeroU64;

    fn settings(token: &str) -> Settings {
        Settings {
            reset_token: token.into(),
            path: "/home/user/.config/furami/settings.json".into(),
            ..Settings::default()
        }
    }

    #[test]
    fn save_dialog_precedes_pending_reset() {
        let settings = settings("7");
        assert_eq!(
            Identity::resolve("save", Some(&settings)),
            Some(Identity::SaveFailure)
        );
        assert_eq!(
            Identity::resolve("warning", Some(&settings)),
            Some(Identity::DurabilityWarning)
        );
    }

    #[test]
    fn reset_requires_a_nonzero_canonical_token() {
        let reset = Identity::resolve("", Some(&settings("9")));
        assert!(matches!(reset, Some(Identity::Reset(token)) if token.get() == 9));
        for token in [
            "",
            "0",
            "00",
            "01",
            "-1",
            " 9",
            "9 ",
            "abc",
            "18446744073709551616",
        ] {
            assert_eq!(
                Identity::resolve("", Some(&settings(token))),
                None,
                "{token}"
            );
        }
        assert_eq!(Identity::resolve("", None), None);
        assert_eq!(Identity::resolve("draft", Some(&settings("9"))), None);
    }

    #[test]
    fn canonical_token_bounds() {
        assert_eq!(canonical_u64("0"), Some(0));
        assert_eq!(canonical_u64("18446744073709551615"), Some(u64::MAX));
        assert_eq!(canonical_u64("18446744073709551616"), None);
        assert_eq!(canonical_u64("+1"), None);
    }

    #[test]
    fn intent_guard_follows_rendered_identity() {
        let token = NonZeroU64::new(5).unwrap();
        let wrong = NonZeroU64::new(6).unwrap();
        let reset = Identity::Reset(token);
        assert!(reset.accepts(Intent::DecideReset {
            token,
            confirmed: false,
        }));
        assert!(reset.accepts(Intent::DecideReset {
            token,
            confirmed: true,
        }));
        assert!(!reset.accepts(Intent::DecideReset {
            token: wrong,
            confirmed: true,
        }));
        assert!(!reset.accepts(Intent::RetrySave));
        assert!(!reset.accepts(Intent::CloseWithoutSave));

        assert!(Identity::SaveFailure.accepts(Intent::RetrySave));
        assert!(Identity::SaveFailure.accepts(Intent::CloseWithoutSave));
        assert!(!Identity::SaveFailure.accepts(Intent::DecideReset {
            token,
            confirmed: true,
        }));

        assert!(Identity::DurabilityWarning.accepts(Intent::CloseWithoutSave));
        assert!(!Identity::DurabilityWarning.accepts(Intent::RetrySave));
    }

    #[test]
    fn popup_slots_match_chrome() {
        assert_eq!(Identity::SaveFailure.popup(), "save");
        assert_eq!(Identity::DurabilityWarning.popup(), "save");
        assert_eq!(
            Identity::Reset(NonZeroU64::new(1).unwrap()).popup(),
            "reset"
        );
    }
}
