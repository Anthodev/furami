//! Companion chrome. The host owns native geometry, input shape, and lifetime.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Instant,
};

use gpui::{
    Bounds, Context, FocusHandle, InteractiveElement as _, IntoElement, KeyDownEvent, KeyUpEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Render,
    Role, ScrollEvent, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity,
    Window, canvas, div, prelude::FluentBuilder as _, px,
};

use super::{
    Action, HostState, RuntimeCoordinator, brume,
    capture_context::{self, CaptureContext},
    chrome::Popup,
    controls::{self, Activate, Activation, HitRegions, OPTIONS_ID, PAUSE_RESUME_ID},
    decisions::{self, DecisionFocus, Identity},
    settings_menu::{Navigate, Section, SettingsMenu},
    top_chrome::{self, FocusTarget, SelectorKind, TopChrome},
    volume,
    x11::HitRect,
};

const MENU_PAUSE_ID: &str = "gate-menu-pause-resume";
const CLOSE_ID: &str = "gate-close-qualification";
const PRIMARY_WIDTH: f32 = 96.;
const OPTIONS_WIDTH: f32 = 104.;
const PANEL_WIDTH: f32 = 216.;

/// A completed GPUI paint, not evidence of compositor presentation. Only the
/// native host may accept the epoch/extent and install its input region.
pub(super) struct CompanionPaint {
    pub(super) epoch: u64,
    pub(super) extent: (u16, u16),
    pub(super) blank: bool,
    pub(super) popup: Option<Popup>,
    pub(super) hits: Vec<HitRect>,
    pub(super) modal: bool,
}

pub(super) type PanelBounds = Rc<Cell<Option<Bounds<Pixels>>>>;

pub(super) struct CompanionView {
    state: Rc<RefCell<HostState>>,
    mailbox: Rc<RefCell<Option<CompanionPaint>>>,
    root: FocusHandle,
    top: Option<TopChrome>,
    menu: Option<SettingsMenu>,
    capture_context: CaptureContext,
    primary: FocusHandle,
    options: FocusHandle,
    window_mode: FocusHandle,
    volume_slider: FocusHandle,
    volume_mute: FocusHandle,
    menu_pause: FocusHandle,
    close: FocusHandle,
    decision_focus: DecisionFocus,
    decision_scroll: gpui::ScrollHandle,
    suspended_focus: Option<SuspendedFocus>,
    _focus_lost: Subscription,
}

struct SuspendedFocus {
    handle: FocusHandle,
    popup: Option<Popup>,
    keyboard: bool,
}

impl CompanionView {
    pub(super) fn new(
        state: Rc<RefCell<HostState>>,
        mailbox: Rc<RefCell<Option<CompanionPaint>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let root = cx.focus_handle();
        root.focus(window, cx);
        // Only the managed root can request application close. Actual native
        // companion destruction still triggers the owner's loss path.
        window.on_window_should_close(cx, |_, _| false);
        let focus_lost = cx.on_focus_lost(window, Self::park_disabled_focus);
        let top = matches!(state.borrow().runtime, RuntimeCoordinator::Capture(_))
            .then(|| TopChrome::new(cx));
        let menu = top.is_some().then(|| SettingsMenu::new(cx));
        Self {
            state,
            mailbox,
            root,
            top,
            menu,
            capture_context: CaptureContext::new(),
            primary: cx.focus_handle(),
            options: cx.focus_handle(),
            window_mode: cx.focus_handle(),
            volume_slider: cx.focus_handle(),
            volume_mute: cx.focus_handle(),
            menu_pause: cx.focus_handle(),
            close: cx.focus_handle(),
            decision_focus: DecisionFocus::new(cx),
            decision_scroll: gpui::ScrollHandle::new(),
            suspended_focus: None,
            _focus_lost: focus_lost,
        }
    }

    fn focus_targets(&self, cx: &gpui::App) -> Vec<FocusTarget> {
        let state = self.state.borrow();
        if !accepts_chrome(&state) {
            return Vec::new();
        }
        if let Some(popup) = state.chrome.popup() {
            if popup.invoker == decisions::RESET_POPUP {
                return vec![
                    focus_target(self.decision_focus.primary()),
                    focus_target(self.decision_focus.secondary()),
                ];
            }
            if let Some(section) = Section::from_popup(popup)
                && let Some(menu) = &self.menu
            {
                return menu.focus_targets(section, &state, cx);
            }
            if popup.invoker == OPTIONS_ID {
                let mut targets = Vec::with_capacity(2);
                if pause_only(state.action()).is_some() {
                    targets.push(focus_target(&self.menu_pause));
                }
                targets.push(focus_target(&self.close));
                return targets;
            }
            if let Some(top) = &self.top {
                let mut targets = top_chrome::chrome_targets(top, &state, cx);
                if targets.is_empty() {
                    targets.push(focus_target(&top_chrome::popup_root(top)));
                }
                return targets;
            }
            return Vec::new();
        }
        if let Some(top) = &self.top {
            let mut targets = top_chrome::chrome_targets(top, &state, cx);
            if state
                .view
                .snapshot
                .as_deref()
                .is_some_and(|snapshot| snapshot.can_set_gain)
            {
                targets.push(focus_target(&self.volume_slider));
                targets.push(focus_target(&self.volume_mute));
            }
            targets.extend(capture_context::targets(&self.capture_context, &state, cx));
            if state.view.snapshot.is_some() {
                targets.push(focus_target(&self.options));
            }
            targets
        } else {
            let mut targets = Vec::with_capacity(3);
            if state.action().is_some() {
                targets.push(focus_target(&self.primary));
            }
            if state.view.snapshot.is_some() {
                targets.push(focus_target(&self.window_mode));
            }
            targets.push(focus_target(&self.options));
            targets
        }
    }

    fn focus_target(
        &self,
        target: &FocusTarget,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        target.handle.focus(window, cx);
        top_chrome::apply_reveal(target);
        if keyboard {
            let mut state = self.state.borrow_mut();
            if target.handle == self.volume_slider || target.handle == self.volume_mute {
                state.volume.keyboard_expand();
            }
            state.chrome.keyboard(true, Instant::now());
        }
        cx.notify();
    }

    pub(super) fn focus_selector(
        &mut self,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.suspended_focus = None;
        let open = selector_popup(self.state.borrow().chrome.popup());
        if open && let Some(target) = self.focus_targets(cx).first() {
            self.focus_target(target, keyboard, window, cx);
        }
    }

    pub(super) fn request_focus(
        &mut self,
        backward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.suspended_focus = None;
        let targets = self.focus_targets(cx);
        let target = if backward {
            targets.last()
        } else {
            targets.first()
        };
        if let Some(target) = target {
            self.focus_target(target, true, window, cx);
        } else {
            self.request_video_focus();
        }
    }

    pub(super) fn dismiss_popup(
        &mut self,
        rendered: Popup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state.borrow().decision_view().identity().is_some() {
            return;
        }
        self.finish_mode_editor_for_popup(rendered, cx);
        if Section::from_popup(rendered) == Some(Section::Rendering) {
            self.finish_renderer(window, cx);
        }
        let restore = {
            let mut state = self.state.borrow_mut();
            if !state.chrome.dismiss(rendered, Instant::now()) {
                return;
            }
            state.stop_top_refresh();
            state.settings_refresh_due = None;
            if !accepts_chrome(&state) {
                None
            } else if Section::from_popup(rendered).is_some() {
                state.focus_intent = true;
                Some(self.options.clone())
            } else if let Some(top) = &self.top {
                let handle = state
                    .view
                    .snapshot
                    .as_ref()
                    .and_then(|_| top_chrome::restore_handle(top, rendered.invoker));
                state.focus_intent |= handle.is_some();
                handle
            } else {
                (rendered.invoker == OPTIONS_ID).then(|| self.options.clone())
            }
        };
        self.suspended_focus = None;
        if let Some(handle) = restore {
            self.focus_target(
                &focus_target(&handle),
                window.last_input_was_keyboard(),
                window,
                cx,
            );
        } else {
            self.request_video_focus();
        }
        cx.notify();
    }

    pub(super) fn finish_mode_editor_for_popup(&self, popup: Popup, cx: &mut Context<Self>) {
        if let Some(group) = self
            .top
            .as_ref()
            .and_then(|top| top.editor_for_popup(popup))
        {
            self.finish_mode_editor(group, cx);
        }
    }

    pub(super) fn finish_mode_editor(&self, group: &FocusHandle, cx: &mut Context<Self>) {
        if let Some(top) = &self.top {
            top.finish_editor(&mut self.state.borrow_mut(), group, cx);
            cx.notify();
        }
    }

    fn request_video_focus(&self) {
        self.state.borrow_mut().return_focus_intent = true;
    }

    pub(super) fn confirm_video_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.suspended_focus = None;
        self.root.focus(window, cx);
        self.state
            .borrow_mut()
            .chrome
            .leave_keyboard(Instant::now());
    }

    fn traverse(&mut self, backward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.suspended_focus = None;
        let focused = window.focused(cx);
        let targets = self.focus_targets(cx);
        let first = if backward {
            targets.last()
        } else {
            targets.first()
        };
        let index =
            focused.and_then(|handle| targets.iter().position(|target| target.handle == handle));
        let next = match index {
            Some(index) => {
                let next = if backward {
                    index.checked_sub(1).and_then(|index| targets.get(index))
                } else {
                    targets.get(index + 1)
                };
                let popup = self.state.borrow().chrome.popup();
                if popup.is_some_and(|popup| popup.invoker != OPTIONS_ID) {
                    next.or(first)
                } else {
                    next
                }
            }
            None => first,
        };
        if let Some(top) = &self.top
            && let Some(group) = top.focused_editor(window, cx)
            && next.is_none_or(|target| !top.editor_has_field(&target.handle, cx))
        {
            self.finish_mode_editor(group, cx);
        }
        if let Some(target) = next {
            self.focus_target(target, true, window, cx);
        } else {
            self.request_video_focus();
        }
    }

    fn park_disabled_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.root.contains_focused(window, cx) {
            return;
        }
        let focused = window.focused(cx);
        let mut state = self.state.borrow_mut();
        self.suspended_focus = focused
            .filter(|focus| {
                accepts_chrome(&state)
                    && !state.return_focus_intent
                    && (*focus == self.primary || *focus == self.menu_pause)
            })
            .map(|handle| SuspendedFocus {
                handle,
                popup: state.chrome.popup(),
                keyboard: window.last_input_was_keyboard(),
            });
        state.chrome.leave_keyboard(Instant::now());
        drop(state);
        self.root.focus(window, cx);
    }

    fn restore_enabled_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.suspended_focus.as_ref() else {
            return;
        };
        let eligible = {
            let state = self.state.borrow();
            accepts_chrome(&state)
                && !state.return_focus_intent
                && state.chrome.popup() == saved.popup
                && self.root.is_focused(window)
        };
        if !eligible {
            self.suspended_focus = None;
            return;
        }
        if saved.handle.is_rendered(window)
            && self
                .focus_targets(cx)
                .iter()
                .any(|target| target.handle == saved.handle)
        {
            let Some(saved) = self.suspended_focus.take() else {
                return;
            };
            saved.handle.focus(window, cx);
            if saved.keyboard && window.last_input_was_keyboard() {
                self.state
                    .borrow_mut()
                    .chrome
                    .keyboard(true, Instant::now());
            }
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        if matches!(event.keystroke.key.as_str(), "f11" | "escape")
            && self.state.borrow().decision_view().identity().is_some()
        {
            cx.stop_propagation();
            return;
        }
        if self
            .menu
            .as_ref()
            .and_then(|menu| menu.active_edit(window))
            .is_some()
        {
            match event.keystroke.key.as_str() {
                "f11" | "escape" => {
                    cx.stop_propagation();
                    return;
                }
                "enter" if !modifiers.modified() => {
                    if !event.is_held {
                        self.finish_renderer(window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if let Some(group) = self
            .top
            .as_ref()
            .and_then(|top| top.focused_editor(window, cx))
        {
            match event.keystroke.key.as_str() {
                "f11" | "escape" => {
                    cx.stop_propagation();
                    return;
                }
                "enter" if !modifiers.modified() => {
                    if !event.is_held {
                        self.finish_mode_editor(group, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                // Space and other printable keys reach the registered GPUI
                // input handler; only the navigation keys below are routed.
                _ => {}
            }
        }
        match event.keystroke.key.as_str() {
            "f11" if !modifiers.modified() => {
                if !event.is_held {
                    self.state.borrow_mut().request_fullscreen();
                }
            }
            "escape" if !modifiers.modified() => {
                let popup = self.state.borrow().chrome.popup();
                if !event.is_held {
                    if let Some(popup) = popup {
                        self.dismiss_popup(popup, window, cx);
                    } else {
                        self.open_menu(Section::Index, false, window, cx);
                    }
                }
            }
            "tab" if modifiers.number_of_modifiers() == u8::from(modifiers.shift) => {
                if !event.is_held {
                    self.traverse(modifiers.shift, window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn open_menu(
        &mut self,
        section: Section,
        accessibility: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        {
            let mut state = self.state.borrow_mut();
            if state.chrome.popup().is_some() || !accepts_chrome(&state) {
                return;
            }
            let now = Instant::now();
            if self.menu.is_some() {
                state.open_capture_menu(section, now);
            } else {
                state.chrome.open_popup(OPTIONS_ID, now);
            }
            state.focus_intent |= accessibility;
        }
        self.suspended_focus = None;
        if let Some(first) = self.focus_targets(cx).first() {
            first.handle.focus(window, cx);
        }
        cx.notify();
    }

    fn finish_renderer(&self, window: &Window, cx: &mut Context<Self>) {
        if let Some(menu) = &self.menu
            && let Some(expected) = menu.active_edit(window)
        {
            menu.finish_edit(expected, &mut self.state.borrow_mut(), cx);
        }
    }

    fn menu_navigation(popup: Popup, cx: &Context<Self>) -> Navigate {
        let weak = cx.weak_entity();
        Rc::new(move |section, source, window, cx| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            view.update(cx, |this, cx| {
                {
                    let state = this.state.borrow();
                    if state.chrome.popup() != Some(popup) || !accepts_chrome(&state) {
                        return;
                    }
                }
                if Section::from_popup(popup) == Some(Section::Rendering) {
                    this.finish_renderer(window, cx);
                }
                {
                    let mut state = this.state.borrow_mut();
                    if state.chrome.popup() != Some(popup) {
                        return;
                    }
                    state.open_capture_menu(section, Instant::now());
                    state.focus_intent |= source == Activation::Accessibility;
                }
                this.suspended_focus = None;
                if let Some(first) = this.focus_targets(cx).first() {
                    this.focus_target(first, source != Activation::Pointer, window, cx);
                }
                cx.notify();
            });
        })
    }

    fn options_action(cx: &Context<Self>) -> Activate {
        let weak = cx.weak_entity();
        Rc::new(move |source, window, cx| {
            if let Some(view) = weak.upgrade() {
                view.update(cx, |view, cx| {
                    view.open_menu(
                        Section::Root,
                        source == Activation::Accessibility,
                        window,
                        cx,
                    );
                });
            }
        })
    }

    fn window_mode_action(cx: &Context<Self>) -> Activate {
        let weak = cx.weak_entity();
        Rc::new(move |source, _, cx| {
            if let Some(view) = weak.upgrade() {
                view.update(cx, |this, cx| {
                    let mut state = this.state.borrow_mut();
                    if state.chrome.popup().is_none() && state.request_fullscreen_from_control() {
                        state.focus_intent |= source == Activation::Accessibility;
                        cx.notify();
                    }
                });
            }
        })
    }

    fn primary_action(
        &self,
        rendered: Option<Action>,
        popup: Option<Popup>,
        cx: &Context<Self>,
    ) -> Option<Activate> {
        let rendered = rendered?;
        let weak = cx.weak_entity();
        Some(Rc::new(move |source, window, cx| {
            if let Some(view) = weak.upgrade() {
                view.update(cx, |this, cx| {
                    {
                        let mut state = this.state.borrow_mut();
                        if !accepts_chrome(&state)
                            || state.chrome.popup() != popup
                            || state.action() != Some(rendered)
                        {
                            return;
                        }
                        state.focus_intent |= source == Activation::Accessibility;
                        state.activate(rendered);
                    }
                    let target = if popup.is_some() {
                        &this.menu_pause
                    } else {
                        &this.primary
                    };
                    target.focus(window, cx);
                    cx.notify();
                });
            }
        }))
    }

    fn close_action(&self, popup: Popup, cx: &Context<Self>) -> Activate {
        let weak = cx.weak_entity();
        Rc::new(move |_, _, cx| {
            if let Some(view) = weak.upgrade() {
                view.update(cx, |this, cx| {
                    let mut state = this.state.borrow_mut();
                    if accepts_chrome(&state) && state.chrome.dismiss(popup, Instant::now()) {
                        state.request_close();
                        cx.notify();
                    }
                });
            }
        })
    }

    /// Paint the published volume rail and wire its dispatch and hover. The
    /// rail records its full extent in the frame's [`HitRegions`], so presses
    /// over it belong to the companion rather than the video child.
    fn volume_rail(
        &self,
        window: &mut Window,
        regions: &HitRegions,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let weak = cx.weak_entity();
        let dispatch: volume::Dispatch = Rc::new(move |intent, source, _window, cx| {
            if let Some(view) = weak.upgrade() {
                view.update(cx, |this, cx| {
                    {
                        let mut state = this.state.borrow_mut();
                        if state.apply_volume(intent) {
                            state.focus_intent |= source == Activation::Accessibility;
                        }
                    }
                    cx.notify();
                });
            }
        });
        let hover: volume::Hover = {
            let weak = cx.weak_entity();
            Rc::new(move |hovered, _, cx| {
                if let Some(view) = weak.upgrade() {
                    view.update(cx, |this, cx| {
                        if this.state.borrow_mut().volume.hover(hovered) {
                            cx.notify();
                        }
                    });
                }
            })
        };
        let state = self.state.borrow();
        let expanded = state
            .volume
            .expanded(self.volume_slider.contains_focused(window, cx));
        let (enabled, percent, muted) =
            state
                .view
                .snapshot
                .as_deref()
                .map_or((false, 0, true), |snapshot| {
                    (
                        snapshot.can_set_gain,
                        snapshot.live.volume_percent,
                        snapshot.live.muted,
                    )
                });
        volume::rail(
            volume::Rail {
                viewport: window.viewport_size(),
                expanded,
                enabled,
                visual_focus: self.volume_slider.contains_focused(window, cx)
                    && window.last_input_was_keyboard(),
                volume_percent: percent,
                muted,
            },
            &self.volume_slider,
            &self.volume_mute,
            regions,
            state.volume.drag_handle(),
            hover,
            dispatch,
        )
        .on_scroll(cx.listener(|this, event: &ScrollEvent, _, cx| {
            let outcome = this.state.borrow_mut().volume_wheel(event, true);
            if outcome != volume::WheelOutcome::Ignore {
                this.state.borrow_mut().chrome.wheel(Instant::now());
                cx.stop_propagation();
                cx.notify();
            }
        }))
    }
}

impl Render for CompanionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (epoch, opacity, popup, action, paused, enabled, qualification, stats_hidden, reset) = {
            let state = self.state.borrow();
            (
                state.overlay_epoch,
                state.overlay_opacity,
                state.chrome.popup(),
                state.action(),
                state.view.paused,
                accepts_chrome(&state),
                matches!(state.runtime, RuntimeCoordinator::Qualification(_)),
                state.view.stats_hidden,
                state
                    .decision_view()
                    .identity()
                    .filter(|identity| matches!(identity, Identity::Reset(_))),
            )
        };
        let regions = HitRegions::new();
        let gate_popup = popup.filter(|popup| popup.invoker == OPTIONS_ID);
        let capture_menu =
            popup.and_then(|popup| Section::from_popup(popup).map(|section| (popup, section)));
        if let Some(menu) = &mut self.menu {
            menu.reconcile(capture_menu.map(|(popup, _)| popup));
        }
        let blank = opacity <= 0.;
        let modal = !blank
            && reset.is_some()
            && popup.is_some_and(|popup| popup.invoker == decisions::RESET_POPUP);
        let mut chrome = div().absolute().inset_0().opacity(opacity);

        let panel = popup.filter(|_| enabled).map(|_| Rc::new(Cell::new(None)));

        if !blank
            && reset.is_none()
            && let Some(top) = &mut self.top
        {
            chrome = chrome.child(top_chrome::paint(
                top,
                &self.state,
                popup,
                enabled,
                panel.as_ref(),
                &regions,
                window,
                cx,
            ));
            if popup.is_none() {
                chrome = chrome.child(self.volume_rail(window, &regions, cx));
            }
            chrome = chrome.child(capture_context::paint(
                &self.capture_context,
                &self.state,
                stats_hidden,
                &regions,
                window,
                cx,
            ));
        }
        if !blank
            && enabled
            && reset.is_none()
            && let Some(menu) = &mut self.menu
        {
            let margin = px(brume::WINDOW_MARGIN);
            let options = controls::button(
                "optionsTrigger",
                "Options",
                brume::icon("adjustments"),
                popup.is_none(),
                &regions,
                popup.is_none().then(|| Self::options_action(cx)),
            )
            .aria_label("Capture options")
            .when(popup.is_none(), |button| button.track_focus(&self.options))
            .absolute()
            .right(margin)
            .bottom(margin);
            chrome = chrome.child(options);
            if let Some((popup, section)) = capture_menu {
                let size = window.viewport_size();
                let bottom = margin + px(brume::CONTROL_HEIGHT + brume::GAP);
                let width = px(section.width() + 2. * brume::EDGE)
                    .min((size.width - margin * 2.).max(px(1.)));
                let height = (size.height - bottom - margin).max(px(1.));
                let body = menu.paint(
                    &self.state,
                    (popup, section),
                    &Self::menu_navigation(popup, cx),
                    &regions,
                    window,
                    cx,
                );
                chrome = chrome.child(
                    div()
                        .id("capture-menu-panel")
                        .absolute()
                        .right(margin)
                        .bottom(bottom)
                        .w(width)
                        .max_h(height)
                        .rounded(px(brume::RADIUS_SURFACE))
                        .border_1()
                        .border_color(brume::DIVIDER)
                        .bg(brume::MENU_SURFACE)
                        .children(panel.as_ref().map(|panel| measure_panel(Rc::clone(panel))))
                        .child(
                            div()
                                .id(section.invoker())
                                .max_h(height)
                                .overflow_y_scroll()
                                .p(px(brume::EDGE))
                                .child(body),
                        ),
                );
            }
        }
        if modal && let Some(identity) = reset {
            let state = self.state.borrow();
            let weak = cx.weak_entity();
            let dispatch: decisions::Dispatch = Rc::new(move |intent, source, _, cx| {
                if let Some(view) = weak.upgrade() {
                    view.update(cx, |this, cx| {
                        let mut state = this.state.borrow_mut();
                        if state.chrome.popup() == popup
                            && state.dispatch_decision(identity, intent)
                        {
                            state.focus_intent |= source == Activation::Accessibility;
                            cx.notify();
                        }
                    });
                }
            });
            if let Some(body) = decisions::paint(
                state.decision_view(),
                &self.decision_focus,
                &regions,
                Some(&self.decision_scroll),
                &dispatch,
            ) {
                let margin = px(brume::WINDOW_MARGIN);
                let width = px(decisions::WIDTH)
                    .min((window.viewport_size().width - margin * 2.).max(px(1.)));
                chrome = chrome
                    .child(div().absolute().inset_0().bg(gpui::rgba(0x00000060)))
                    .child(
                        div()
                            .id("settings-reset-modal")
                            .absolute()
                            .right(margin)
                            .bottom(margin)
                            .w(width)
                            .p(px(brume::EDGE))
                            .rounded(px(brume::RADIUS_SURFACE))
                            .border_1()
                            .border_color(brume::DIVIDER)
                            .bg(brume::MENU_SURFACE)
                            .children(panel.as_ref().map(|panel| measure_panel(Rc::clone(panel))))
                            .child(body),
                    );
            }
        }
        if !blank && qualification {
            let (label, icon) = primary_presentation(action, paused);
            let margin = px(brume::WINDOW_MARGIN);
            let hit = px(brume::HIT_TARGET);
            let fullscreen = self.state.borrow().view.fullscreen;
            let window_available = enabled && gate_popup.is_none();
            let (window_label, window_icon) = if fullscreen {
                ("Windowed", "minimize")
            } else {
                ("Fullscreen", "maximize")
            };
            chrome = chrome.child(
                controls::button(
                    "qualification-window-mode",
                    window_label,
                    brume::icon(window_icon),
                    window_available,
                    &regions,
                    window_available.then(|| Self::window_mode_action(cx)),
                )
                .when(window_available, |button| {
                    button.track_focus(&self.window_mode)
                })
                .absolute()
                .left(margin)
                .top(margin)
                .h(hit),
            );
            let ribbon = div()
                .absolute()
                .right(margin)
                .top(margin)
                .flex()
                .gap(px(brume::GAP))
                .rounded(px(brume::RADIUS_SURFACE))
                .bg(brume::SURFACE);
            if let Some(popup) = gate_popup {
                // No underlying actionable ribbon nodes or input regions while
                // the panel is open. Outside presses go to the managed root.
                if qualification {
                    chrome = chrome.child(
                        ribbon
                            .child(
                                div()
                                    .w(px(PRIMARY_WIDTH))
                                    .h(hit)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(label),
                            )
                            .child(
                                div()
                                    .w(px(OPTIONS_WIDTH))
                                    .h(hit)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child("Options"),
                            )
                            .text_color(brume::FOREGROUND),
                    );
                }
                let menu_action = pause_only(action);
                chrome = chrome.child(
                    div()
                        .id("gate-options-popup")
                        .accessibility_id("gate-options-popup")
                        .role(Role::Group)
                        .aria_label("Options")
                        .absolute()
                        .right(margin)
                        .top(margin + hit + margin)
                        .w(px(PANEL_WIDTH))
                        .p(margin)
                        .flex()
                        .flex_col()
                        .rounded(px(brume::RADIUS_SURFACE))
                        .bg(brume::MENU_SURFACE)
                        .children(panel.as_ref().map(|panel| measure_panel(Rc::clone(panel))))
                        .child(
                            controls::button(
                                MENU_PAUSE_ID,
                                if paused { "Resume" } else { "Pause" },
                                brume::icon(if paused { "play" } else { "pause" }),
                                menu_action.is_some(),
                                &regions,
                                self.primary_action(menu_action, Some(popup), cx),
                            )
                            .when(menu_action.is_some(), |button| {
                                button.track_focus(&self.menu_pause)
                            })
                            .h(hit),
                        )
                        .child(
                            controls::button(
                                CLOSE_ID,
                                if qualification {
                                    "Close qualification"
                                } else {
                                    "Close Furami"
                                },
                                brume::icon("x"),
                                enabled,
                                &regions,
                                enabled.then(|| self.close_action(popup, cx)),
                            )
                            .when(enabled, |button| button.track_focus(&self.close))
                            .h(hit),
                        ),
                );
            } else {
                let weak = cx.weak_entity();
                let options: Activate = Rc::new(move |source, window, cx| {
                    if let Some(view) = weak.upgrade() {
                        view.update(cx, |this, cx| {
                            this.open_menu(
                                Section::Root,
                                source == Activation::Accessibility,
                                window,
                                cx,
                            );
                        });
                    }
                });
                chrome = chrome.child(
                    ribbon
                        .child(
                            controls::button(
                                PAUSE_RESUME_ID,
                                label,
                                brume::icon(icon),
                                action.is_some(),
                                &regions,
                                self.primary_action(action, None, cx),
                            )
                            .when(action.is_some(), |button| button.track_focus(&self.primary))
                            .w(px(PRIMARY_WIDTH))
                            .h(hit),
                        )
                        .child(
                            controls::button(
                                OPTIONS_ID,
                                "Options",
                                brume::icon("settings"),
                                enabled,
                                &regions,
                                enabled.then_some(options),
                            )
                            .when(enabled, |button| button.track_focus(&self.options))
                            .w(px(OPTIONS_WIDTH))
                            .h(hit),
                        ),
                );
            }
        }
        let pointer_panel = panel.clone();

        div()
            .id("furami-companion")
            .size_full()
            .relative()
            .track_focus(&self.root)
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, cx| {
                if event.keystroke.key == "f11" {
                    this.state.borrow_mut().f11_release();
                    cx.stop_propagation();
                }
            }))
            .capture_any_mouse_down(
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if let Some(popup) = popup
                        && pointer_panel
                            .as_ref()
                            .and_then(|panel| panel.get())
                            .is_some_and(|bounds| !bounds.contains(&event.position))
                    {
                        this.dismiss_popup(popup, window, cx);
                        cx.stop_propagation();
                        return;
                    }
                    let mut state = this.state.borrow_mut();
                    if event.button == MouseButton::Left {
                        state.volume.cancel_drag();
                    }
                    if event.button == MouseButton::Left && accepts_chrome(&state) {
                        this.suspended_focus = None;
                        state.chrome.leave_keyboard(Instant::now());
                        state.chrome.press(true, Instant::now());
                        state.focus_intent = true;
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    let mut state = this.state.borrow_mut();
                    state.volume.cancel_drag();
                    state.chrome.press(false, Instant::now());
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    let mut state = this.state.borrow_mut();
                    state.volume.cancel_drag();
                    state.chrome.press(false, Instant::now());
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, _| {
                let mut state = this.state.borrow_mut();
                if event.pressed_button != Some(MouseButton::Left) {
                    state.volume.cancel_drag();
                }
                state.volume.pointer();
                state.chrome.pointer(event.position, Instant::now());
            }))
            .child(chrome)
            .child(paint_marker(
                epoch,
                blank,
                popup,
                panel,
                regions,
                modal,
                cx.weak_entity(),
            ))
    }
}

fn focus_target(handle: &FocusHandle) -> FocusTarget {
    FocusTarget::plain(handle)
}

fn selector_popup(popup: Option<Popup>) -> bool {
    popup.is_some_and(|popup| SelectorKind::from_invoker(popup.invoker).is_some())
}

fn accepts_chrome(state: &HostState) -> bool {
    state.accepts_input() && state.view.close_dialog.is_empty() && state.overlay_opacity > 0.
}

fn pause_only(action: Option<Action>) -> Option<Action> {
    action.filter(|action| matches!(action, Action::TogglePause(..)))
}

fn primary_presentation(action: Option<Action>, paused: bool) -> (&'static str, &'static str) {
    match action {
        Some(Action::Open) => ("Open", "play"),
        _ if paused => ("Resume", "play"),
        _ => ("Pause", "pause"),
    }
}

fn hit_rect(bounds: Bounds<Pixels>) -> HitRect {
    HitRect {
        left: bounds.origin.x.into(),
        top: bounds.origin.y.into(),
        right: (bounds.origin.x + bounds.size.width).into(),
        bottom: (bounds.origin.y + bounds.size.height).into(),
    }
}

pub(super) fn measure_panel(panel: PanelBounds) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), _, _| panel.set(Some(bounds)),
    )
    .absolute()
    .inset_0()
}

fn paint_marker(
    epoch: u64,
    blank: bool,
    popup: Option<Popup>,
    panel: Option<PanelBounds>,
    regions: HitRegions,
    modal: bool,
    view: WeakEntity<CompanionView>,
) -> impl IntoElement {
    // A final sibling, so all the interactive elements have gone through
    // layout and paint. The marker itself has no input, color, or a11y node.
    canvas(
        |_, _, _| (),
        move |_, (), window, _| {
            let size = window.viewport_size();
            let scale = window.scale_factor();
            let Some(extent) = device_extent(size.width, size.height, scale) else {
                return;
            };
            let hits = if blank {
                Vec::new()
            } else if popup.is_some() {
                panel
                    .and_then(|panel| panel.get())
                    .map(hit_rect)
                    .into_iter()
                    .collect()
            } else {
                regions.rects().into_iter().map(hit_rect).collect()
            };
            window.on_next_frame(move |window, app| {
                // This callback is scheduled from actual paint, not render.
                // The host must still check native bounds, mapping, and epoch.
                if let Some(view) = view.upgrade() {
                    view.update(app, |view, cx| {
                        let current = {
                            let state = view.state.borrow();
                            state.overlay_epoch == epoch
                                && (state.overlay_opacity <= 0.) == blank
                                && state.chrome.popup() == popup
                        };
                        if current {
                            *view.mailbox.borrow_mut() = Some(CompanionPaint {
                                epoch,
                                extent,
                                blank,
                                popup,
                                hits,
                                modal,
                            });
                            view.restore_enabled_focus(window, cx);
                            if !blank
                                && selector_popup(popup)
                                && let Some(top) = &view.top
                                && !top_chrome::popup_root(top).contains_focused(window, cx)
                            {
                                let keyboard = window.last_input_was_keyboard();
                                view.focus_selector(keyboard, window, cx);
                            }
                        }
                    });
                }
            });
        },
    )
    .absolute()
    .inset_0()
}

fn device_extent(width: Pixels, height: Pixels, scale: f32) -> Option<(u16, u16)> {
    fn axis(length: Pixels, scale: f32) -> Option<u16> {
        let device = (f32::from(length) * scale).round();
        (1. ..=f32::from(u16::MAX))
            .contains(&device)
            .then_some(device as u16)
    }
    Some((axis(width, scale)?, axis(height, scale)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpui_host_companion_extent_rounds_logical_size_at_fractional_scale() {
        assert_eq!(device_extent(px(480.), px(540.), 1.5), Some((720, 810)));
        assert_eq!(device_extent(px(641.), px(401.), 1.5), Some((962, 602)));
    }

    #[test]
    fn gpui_host_companion_extent_refuses_impossible_native_sizes() {
        assert_eq!(device_extent(px(0.), px(540.), 1.), None);
        assert_eq!(device_extent(px(800.), px(540.), f32::NAN), None);
        assert_eq!(device_extent(px(40_000.), px(540.), 2.), None);
    }
}
