//! Capture settings navigation inside the companion's single popup surface.

use std::{cell::RefCell, rc::Rc, time::Instant};

use gpui::{
    AnyElement, App, Context, Div, FocusHandle, Role, SharedString, Stateful, Window, div,
    prelude::*, px,
};

use crate::{domain::render::ScalerChoice, ui::projection::Snapshot};

use super::{
    HostState, brume,
    chrome::Popup,
    companion_view::CompanionView,
    controls::{self, Activate, Activation, HitRegions, LabelVariant},
    device_settings::{self, DeviceSettings, Page as DevicePage},
    playback_menu::{self, MenuInputs, PlaybackMenu},
    preferences_page::{self, PreferencesPage},
    renderer_page::RendererPage,
    top_chrome::FocusTarget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Section {
    Root,
    Index,
    Capture,
    Audio,
    Rendering,
    Preferences,
}

impl Section {
    pub(super) const fn invoker(self) -> &'static str {
        match self {
            Self::Root => "menu",
            Self::Index => "menu-settings",
            Self::Capture => "menu-capture",
            Self::Audio => "menu-audio",
            Self::Rendering => "menu-rendering",
            Self::Preferences => "menu-preferences",
        }
    }

    pub(super) fn from_popup(popup: Popup) -> Option<Self> {
        Some(match popup.invoker {
            "menu" => Self::Root,
            "menu-settings" => Self::Index,
            "menu-capture" => Self::Capture,
            "menu-audio" => Self::Audio,
            "menu-rendering" => Self::Rendering,
            "menu-preferences" => Self::Preferences,
            _ => return None,
        })
    }

    pub(super) const fn back(self) -> Self {
        match self {
            Self::Root | Self::Index => Self::Root,
            _ => Self::Index,
        }
    }

    pub(super) const fn width(self) -> f32 {
        // MenuView applies this factor to the ordinary Brume widths.
        1.2 * match self {
            Self::Root | Self::Index => brume::MENU_WIDTH,
            _ => brume::SETTINGS_WIDTH,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Root => "Capture",
            Self::Index => "Settings",
            Self::Capture => "Capture — format",
            Self::Audio => "Audio",
            Self::Rendering => "Rendering",
            Self::Preferences => "Preferences",
        }
    }
}

pub(super) type Navigate = Rc<dyn Fn(Section, Activation, &mut Window, &mut App)>;

pub(super) struct SettingsMenu {
    popup: Option<Popup>,
    back: FocusHandle,
    entries: [FocusHandle; 4],
    playback: PlaybackMenu,
    devices: DeviceSettings,
    renderer: RendererPage,
    preferences: PreferencesPage,
}

impl SettingsMenu {
    pub(super) fn new(cx: &mut Context<CompanionView>) -> Self {
        Self {
            popup: None,
            back: cx.focus_handle(),
            entries: std::array::from_fn(|_| cx.focus_handle()),
            playback: PlaybackMenu::new(cx),
            devices: DeviceSettings::new(cx),
            renderer: RendererPage::default(),
            preferences: PreferencesPage::new(cx),
        }
    }

    pub(super) fn focus_targets(
        &self,
        section: Section,
        state: &HostState,
        cx: &App,
    ) -> Vec<FocusTarget> {
        if section == Section::Root {
            return self.playback.focus_targets(state);
        }
        let mut targets = vec![FocusTarget::plain(&self.back)];
        match section {
            Section::Index => targets.extend(self.entries.iter().map(FocusTarget::plain)),
            Section::Capture | Section::Audio => targets.extend(device_settings::focus_targets(
                &self.devices,
                state,
                device_page(section),
                cx,
            )),
            Section::Rendering => {
                targets.extend(self.renderer.focus_handles().iter().map(FocusTarget::plain))
            }
            Section::Preferences => targets.extend(self.preferences.focus_targets(state)),
            Section::Root => unreachable!(),
        }
        targets
    }

    pub(super) fn active_edit(&self, window: &Window) -> Option<&FocusHandle> {
        self.renderer.active_edit(window)
    }

    pub(super) fn finish_edit(&self, expected: &FocusHandle, state: &mut HostState, cx: &mut App) {
        self.renderer.finish_edit(expected, state, cx);
    }

    pub(super) fn reconcile(&mut self, popup: Option<Popup>) {
        if self.popup != popup {
            self.popup = popup;
            self.devices.close_open_choice();
            self.renderer.clear();
            self.preferences.reset_disclosure();
        }
    }

    pub(super) fn paint(
        &mut self,
        state: &Rc<RefCell<HostState>>,
        (popup, section): (Popup, Section),
        navigate: &Navigate,
        regions: &HitRegions,
        window: &mut Window,
        cx: &mut Context<CompanionView>,
    ) -> AnyElement {
        if section == Section::Root {
            let on_settings = to_section(navigate, Section::Index);
            return playback_menu::paint(
                &self.playback,
                state,
                &MenuInputs {
                    popup,
                    on_settings: &on_settings,
                },
                regions,
                cx,
            )
            .into_any_element();
        }
        let snapshot = state.borrow().view.snapshot.clone();
        let body = match section {
            Section::Index => self.index(snapshot.as_deref(), regions, navigate),
            Section::Capture | Section::Audio => device_settings::paint(
                &self.devices,
                state,
                device_page(section),
                popup,
                regions,
                cx,
            ),
            Section::Rendering => {
                if let Some(snapshot) = snapshot.as_deref() {
                    self.renderer
                        .paint(popup, snapshot, state, regions, window, cx)
                } else {
                    div()
                }
            }
            Section::Preferences => div().child(preferences_page::paint(
                &self.preferences,
                state,
                popup,
                regions,
                cx,
            )),
            Section::Root => unreachable!(),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(brume::GAP))
            .child(self.header(section, regions, navigate))
            .child(body)
            .into_any_element()
    }

    fn header(&self, section: Section, regions: &HitRegions, navigate: &Navigate) -> Div {
        let destination = section.back();
        let back = controls::button(
            "menuBack",
            "Back",
            None,
            true,
            regions,
            Some(to_section(navigate, destination)),
        )
        .aria_label(if destination == Section::Root {
            "Back to menu"
        } else {
            "Back to settings"
        })
        .w(px(brume::HIT_TARGET + 2. * brume::EDGE))
        .track_focus(&self.back);
        heading(section.title(), env!("CARGO_PKG_VERSION"), Some(back))
    }

    pub(super) fn index(
        &self,
        snapshot: Option<&Snapshot>,
        regions: &HitRegions,
        navigate: &Navigate,
    ) -> Div {
        let selection = snapshot.and_then(|snapshot| snapshot.selection.as_ref());
        let capture = selection.map_or("No source", |selection| {
            nonempty(&selection.source_label, "No source")
        });
        let audio = selection
            .filter(|selection| selection.audio_enabled)
            .map_or("Disabled", |selection| {
                nonempty(&selection.audio_input_label, "Input to choose")
            });
        let rendering = selection.map_or_else(
            || "—".to_owned(),
            |selection| {
                let values = selection.renderer.values();
                let downscale = match values.dscale {
                    ScalerChoice::Inherit => "auto",
                    ScalerChoice::Kernel(kernel) => kernel.as_str(),
                };
                format!("{} / {downscale}", values.scale.as_str())
            },
        );
        let preferences =
            snapshot.map_or("—", |snapshot| nonempty(&snapshot.settings.status, "—"));
        let rows = [
            (
                Section::Capture,
                "settingsCaptureEntry",
                "Capture",
                "Capture settings",
                "camera",
                capture,
            ),
            (
                Section::Audio,
                "settingsAudioEntry",
                "Audio",
                "Audio settings",
                "headphones",
                audio,
            ),
            (
                Section::Rendering,
                "settingsRendererEntry",
                "Rendering",
                "Rendering settings",
                "palette",
                rendering.as_str(),
            ),
            (
                Section::Preferences,
                "settingsPreferencesEntry",
                "Preferences",
                "Preferences",
                "adjustments",
                preferences,
            ),
        ];
        div().flex().flex_col().gap(px(brume::GAP_SMALL)).children(
            rows.into_iter().zip(&self.entries).map(
                |((section, id, label, accessible, icon, summary), focus)| {
                    let summary: SharedString = summary.to_owned().into();
                    controls::button_frame(
                        id,
                        accessible,
                        brume::icon(icon),
                        true,
                        regions,
                        Some(to_section(navigate, section)),
                    )
                    .h(px(52.))
                    .min_w(px(0.))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .flex_col()
                            .min_w(px(0.))
                            .text_center()
                            .child(div().truncate().child(label))
                            .child(div().truncate().child(summary.clone())),
                    )
                    .aria_description(summary)
                    .track_focus(focus)
                },
            ),
        )
    }
}

pub(super) fn heading(
    title: &'static str,
    version: &'static str,
    back: Option<Stateful<Div>>,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(brume::GAP))
        .children(back)
        .child(
            controls::label(title, LabelVariant::Title)
                .id("menuTitle")
                .flex_1()
                .truncate(),
        )
        .child(
            div()
                .id("menuVersion")
                .flex_none()
                .role(Role::Label)
                .aria_label(format!("Version {version}"))
                .text_size(px(brume::FONT_SIZE_SMALL))
                .text_color(brume::SECONDARY)
                .child(format!("v{version}")),
        )
}

fn device_page(section: Section) -> DevicePage {
    match section {
        Section::Capture => DevicePage::Capture,
        Section::Audio => DevicePage::Audio,
        _ => unreachable!(),
    }
}

impl HostState {
    pub(super) fn open_capture_menu(&mut self, section: Section, now: Instant) {
        self.volume.reset();
        self.chrome.open_popup(section.invoker(), now);
        self.redraw = true;
        self.settings_refresh_due = None;
        if matches!(section, Section::Capture | Section::Audio)
            && self
                .chrome
                .popup()
                .is_some_and(|popup| popup.invoker == section.invoker())
        {
            self.settings_refresh_due = Some(now + std::time::Duration::from_secs(2));
            let update = self.runtime.refresh_catalogs();
            self.consume(update);
        }
    }

    pub(super) fn tick_settings_refresh(&mut self, now: Instant) {
        let Some(due) = self.settings_refresh_due else {
            return;
        };
        if !matches!(
            self.chrome.popup().and_then(Section::from_popup),
            Some(Section::Capture | Section::Audio)
        ) {
            self.settings_refresh_due = None;
        } else if now >= due && self.accepts_input() && self.overlay_opacity > 0. {
            self.settings_refresh_due = Some(now + std::time::Duration::from_secs(2));
            let update = self.runtime.refresh_catalogs();
            self.consume(update);
        }
    }
}

pub(super) fn to_section(navigate: &Navigate, section: Section) -> Activate {
    let navigate = Rc::clone(navigate);
    Rc::new(move |source, window, cx| navigate(section, source, window, cx))
}

fn nonempty<'a>(text: &'a str, fallback: &'a str) -> &'a str {
    if text.is_empty() { fallback } else { text }
}
