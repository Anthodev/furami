//! Visibility is driven by real activity and projected policy, not repaint frequency.
use std::time::{Duration, Instant};

use super::brume::{FADE_DURATION, fade_out_cubic};
use crate::ui::projection::Snapshot;
use gpui::{Pixels, Point};

const IDLE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mandatory {
    #[default]
    None,
    IntentError,
    Diagnostic,
    Disconnected,
    Recovery,
    Opening,
}

impl Mandatory {
    fn projected(snapshot: Option<&Snapshot>) -> Self {
        let Some(view) = snapshot else {
            return Self::None;
        };
        if view.intent_error.is_some() {
            Self::IntentError
        } else if view.diagnostics.has_problem {
            Self::Diagnostic
        } else if view.product_phase == "Disconnected" {
            Self::Disconnected
        } else if !view.recovery.stage.is_empty() {
            Self::Recovery
        } else if view.opening {
            Self::Opening
        } else {
            Self::None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Visibility {
    Hidden,
    Held,
    Shown(Instant),
    Fading(Instant),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Popup {
    pub(super) epoch: u64,
    pub(super) invoker: &'static str,
}

pub(super) struct ChromeState {
    visibility: Visibility,
    reveal: Option<(Instant, f32)>,
    available: bool,
    auto_hide: bool,
    mandatory: Mandatory,
    pointer: Option<Point<Pixels>>,
    pressed: bool,
    keyboard_focus: bool,
    popup: Option<Popup>,
    next_epoch: u64,
}

impl Default for ChromeState {
    fn default() -> Self {
        Self {
            visibility: Visibility::Hidden,
            reveal: None,
            available: false,
            auto_hide: false,
            mandatory: Mandatory::None,
            pointer: None,
            pressed: false,
            keyboard_focus: false,
            popup: None,
            next_epoch: 0,
        }
    }
}

impl ChromeState {
    fn holds(&self) -> bool {
        !self.auto_hide || self.pressed || self.keyboard_focus || self.popup.is_some()
    }

    fn reveal(&mut self, now: Instant) {
        if self.available {
            match self.visibility {
                Visibility::Hidden => self.reveal = Some((now, 0.)),
                Visibility::Fading(start) => self.reveal = Some((now, 1. - easing(start, now))),
                _ => {}
            }
            self.visibility = if self.holds() {
                Visibility::Held
            } else {
                Visibility::Shown(now + IDLE)
            };
        }
    }

    fn reconcile_holds(&mut self, now: Instant) {
        match (self.visibility, self.holds()) {
            (Visibility::Shown(_) | Visibility::Fading(_), true) => self.reveal(now),
            (Visibility::Held, false) => self.reveal(now),
            _ => {}
        }
    }

    pub(super) fn reconcile(&mut self, available: bool, snapshot: Option<&Snapshot>, now: Instant) {
        let appeared = available && !self.available;
        self.available = available;
        let mandatory = Mandatory::projected(snapshot);
        let reveal = appeared || (mandatory != self.mandatory && mandatory != Mandatory::None);
        self.mandatory = mandatory;
        self.auto_hide = snapshot.is_some_and(|view| {
            view.product_phase == "Active"
                && view.live.playback_status == "Live"
                && !view.paused
                && !view.opening
                && !view.closing
                && view.intent_error.is_none()
                && !view.output.needs_action
                && !view.diagnostics.has_problem
        });
        if !available {
            self.visibility = Visibility::Hidden;
            self.reveal = None;
            self.pointer = None;
            self.pressed = false;
            self.keyboard_focus = false;
            self.popup = None;
        } else if reveal {
            self.reveal(now);
        } else {
            self.reconcile_holds(now);
        }
    }

    pub(super) fn pointer(&mut self, position: Point<Pixels>, now: Instant) {
        // The X11 backend's Enter event does not synthesize MouseMove;
        // the first delivered motion is activity, not a hover sample.
        if self.available
            && self
                .pointer
                .replace(position)
                .is_none_or(|old| old != position)
        {
            self.keyboard_focus = false;
            self.reveal(now);
        }
    }

    pub(super) fn press(&mut self, pressed: bool, now: Instant) {
        if self.available && self.pressed != pressed {
            self.pressed = pressed;
            if pressed {
                self.reveal(now);
            } else {
                self.reconcile_holds(now);
            }
        }
    }

    pub(super) fn keyboard(&mut self, focused_enabled: bool, now: Instant) {
        if self.available {
            self.keyboard_focus = focused_enabled;
            self.reveal(now);
        }
    }

    pub(super) fn leave_keyboard(&mut self, now: Instant) {
        self.keyboard_focus = false;
        self.reconcile_holds(now);
    }

    pub(super) fn popup(&self) -> Option<Popup> {
        self.popup
    }

    pub(super) fn open_popup(&mut self, invoker: &'static str, now: Instant) {
        if self.available {
            self.next_epoch += 1;
            self.popup = Some(Popup {
                epoch: self.next_epoch,
                invoker,
            });
            self.reveal(now);
        }
    }

    pub(super) fn dismiss(&mut self, rendered: Popup, now: Instant) -> bool {
        if self.popup != Some(rendered) {
            return false;
        }
        self.popup = None;
        self.reconcile_holds(now);
        true
    }

    /// A qualifying wheel event is real pointer activity: it reveals the chrome
    /// and restarts the whole deadline, exactly like a pointer move.
    pub(super) fn wheel(&mut self, now: Instant) {
        self.keyboard_focus = false;
        self.reveal(now);
    }

    pub(super) fn opacity(&mut self, now: Instant) -> f32 {
        if let Visibility::Shown(deadline) = self.visibility
            && now >= deadline
        {
            self.visibility = Visibility::Fading(deadline);
        }
        match self.visibility {
            Visibility::Hidden => 0.,
            Visibility::Held | Visibility::Shown(_) => {
                if let Some((start, initial)) = self.reveal {
                    let amount = easing(start, now);
                    if amount == 1. {
                        self.reveal = None;
                    }
                    initial + (1. - initial) * amount
                } else {
                    1.
                }
            }
            Visibility::Fading(start) => {
                let amount = easing(start, now);
                if amount == 1. {
                    self.visibility = Visibility::Hidden;
                }
                1. - amount
            }
        }
    }
}

fn easing(start: Instant, now: Instant) -> f32 {
    let elapsed = now.saturating_duration_since(start);
    fade_out_cubic((elapsed.as_secs_f32() / FADE_DURATION.as_secs_f32()).min(1.))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};

    #[test]
    fn gpui_host_stationary_pointer_cannot_postpone_fade() {
        let start = Instant::now();
        let mut chrome = ChromeState {
            available: true,
            auto_hide: true,
            ..ChromeState::default()
        };
        let position = point(px(1.), px(1.));
        chrome.pointer(position, start);
        chrome.pointer(point(px(2.), px(1.)), start);
        chrome.pointer(
            point(px(2.), px(1.)),
            start + IDLE - Duration::from_millis(1),
        );
        let fading = chrome.opacity(start + IDLE + FADE_DURATION / 2);
        assert!(fading > 0. && fading < 1.);
        assert_eq!(chrome.opacity(start + IDLE + FADE_DURATION), 0.);
        chrome.pointer(position, start + IDLE + FADE_DURATION);
        assert_eq!(chrome.opacity(start + IDLE + FADE_DURATION * 2), 1.);
    }

    #[test]
    fn gpui_host_first_native_motion_reveals_after_idle() {
        let now = Instant::now();
        let mut chrome = ChromeState::default();
        chrome.reconcile(true, None, now);
        chrome.auto_hide = true;
        chrome.reconcile_holds(now);
        let hidden = now + IDLE + FADE_DURATION;
        assert_eq!(chrome.opacity(hidden), 0.);
        chrome.pointer(point(px(20.), px(20.)), hidden);
        assert_eq!(chrome.opacity(hidden + FADE_DURATION), 1.);
    }

    #[test]
    fn gpui_host_stale_dismiss_cannot_remove_replacement_popup() {
        let now = Instant::now();
        let mut chrome = ChromeState::default();
        chrome.reconcile(true, None, now);
        chrome.open_popup("options", now);
        let stale = chrome.popup().unwrap();
        chrome.open_popup("options", now);
        assert!(!chrome.dismiss(stale, now));
        assert!(chrome.popup().is_some());
        chrome.reconcile(false, None, now);
        assert!(chrome.popup().is_none());
        assert_eq!(chrome.opacity(now), 0.);
    }
}
