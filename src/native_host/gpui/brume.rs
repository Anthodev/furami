//! Brume design tokens for the GPUI frontend.
//!
//! This is the single translation of `qml/Brume.qml` into Rust. The palette
//! swatches are Catppuccin Macchiato (MIT, © Catppuccin) and are the only
//! source of colour truth: every role and derived surface below comes from
//! them so the two frontends cannot drift into a second theme. Prune and Craie
//! remain future preferences only and are deliberately not implemented here.
//!
//! GPUI's default system font is the equivalent of QML's `"Sans Serif"`
//! family; text sizes are the shared 13/11/15 scale, so no family string is
//! forced on the platform text system.
//!
//! The bundled SVG icons live in `assets/icons/` and are included here as the
//! same bytes the QML resource module ships, attribution file included.

// Design tokens are the shared visual contract for every GPUI view. The
// native-composition gate consumes a subset today; the selector, playback and
// settings views consume the rest as they land, and one dictionary is what
// keeps the two frontends from growing a second theme. The allowance is scoped
// to this token module only.
#![allow(
    dead_code,
    reason = "shared Brume token contract; the gate uses a subset"
)]

use std::time::Duration;

use gpui::{FontWeight, Rgba};

/// Opaque RGBA from an `0xRRGGBB` swatch.
const fn color(hex: u32) -> Rgba {
    rgba(hex, 1.0)
}

/// RGBA from an `0xRRGGBB` swatch and an explicit alpha.
const fn rgba(hex: u32, alpha: f32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: alpha,
    }
}

// ── Catppuccin Macchiato swatches ──────────────────────────────────────────
// crust is the deepest pane; subtext0 the dimmer secondary; overlay0/overlay2
// are decorative and not needed by any surface today, so not declared.

pub(super) const BASE: Rgba = color(0x24273a);
pub(super) const MANTLE: Rgba = color(0x1e2030);
pub(super) const SURFACE0: Rgba = color(0x363a4f);
pub(super) const SURFACE1: Rgba = color(0x494d64);
pub(super) const SURFACE2: Rgba = color(0x5b6078);
pub(super) const OVERLAY1: Rgba = color(0x8087a2);
pub(super) const TEXT: Rgba = color(0xcad3f5);
pub(super) const SUBTEXT1: Rgba = color(0xb8c0e0);
pub(super) const BLUE: Rgba = color(0x8aadf4);
pub(super) const RED: Rgba = color(0xed8796);

// ── Role aliases (stable names consumed by the controls) ───────────────────
// window & menus = base; ribbon & surfaces = surface0; controls = surface0
// with hover surface1; borders = surface2; body = text; secondary = subtext1;
// accent = blue. `MATTE` is also the ink drawn on an accent fill.

pub(super) const MATTE: Rgba = BASE;
pub(super) const SURFACE: Rgba = SURFACE0;
pub(super) const FOREGROUND: Rgba = TEXT;
pub(super) const SECONDARY: Rgba = SUBTEXT1;
pub(super) const ACCENT: Rgba = BLUE;
pub(super) const SEPARATOR: Rgba = SURFACE2;
/// Semantic error status (Catppuccin red).
pub(super) const ERROR: Rgba = RED;

// ── Derived surfaces ───────────────────────────────────────────────────────
pub(super) const WINDOW: Rgba = MATTE;
pub(super) const CONTROL: Rgba = SURFACE0;
pub(super) const CONTROL_HOVER: Rgba = SURFACE1;
/// `Qt.darker(surface0, 1.12)`: HSV value divided by 1.12, rounded per channel.
pub(super) const CONTROL_PRESSED: Rgba = color(0x303447);
pub(super) const MENU_SURFACE: Rgba = BASE;
/// `matte` at 78 % opacity.
pub(super) const SCRIM: Rgba = rgba(0x24273a, 0.78);
pub(super) const TRACK: Rgba = MANTLE;
pub(super) const TRACK_FILL: Rgba = ACCENT;
pub(super) const DISABLED_FILL: Rgba = CONTROL_PRESSED;
pub(super) const DISABLED_TEXT: Rgba = OVERLAY1;
/// `separator` at 75 % opacity.
pub(super) const DIVIDER: Rgba = rgba(0x5b6078, 0.75);
/// Black at 28 % opacity.
pub(super) const SHADOW: Rgba = rgba(0x000000, 0.28);

// ── Geometry (logical pixels) ──────────────────────────────────────────────
pub(super) const RADIUS_SURFACE: f32 = 10.0;
pub(super) const RADIUS_CONTROL: f32 = 7.0;
pub(super) const GAP_SMALL: f32 = 4.0;
pub(super) const GAP: f32 = 8.0;
pub(super) const GAP_LARGE: f32 = 16.0;
pub(super) const EDGE: f32 = 10.0;
pub(super) const WINDOW_MARGIN: f32 = 8.0;
pub(super) const SEPARATOR_WIDTH: f32 = 1.0;
pub(super) const DIVIDER_WIDTH: f32 = 1.0;

// ── Controls ───────────────────────────────────────────────────────────────
pub(super) const CONTROL_HEIGHT: f32 = 36.0;
pub(super) const HIT_TARGET: f32 = 48.0;
pub(super) const ICON_SIZE: f32 = 18.0;
pub(super) const RIBBON_ICON_SIZE: f32 = 20.0;
pub(super) const RIBBON_HEIGHT: f32 = 48.0;
pub(super) const VOLUME_WIDTH: f32 = 44.0;
pub(super) const POPUP_MAX_WIDTH: f32 = 420.0;
pub(super) const MENU_WIDTH: f32 = 320.0;
pub(super) const SETTINGS_WIDTH: f32 = 400.0;
pub(super) const POPUP_MAX_LIST_HEIGHT: f32 = 320.0;

// ── Type ───────────────────────────────────────────────────────────────────
pub(super) const FONT_SIZE: f32 = 13.0;
pub(super) const FONT_SIZE_SMALL: f32 = 11.0;
pub(super) const FONT_SIZE_TITLE: f32 = 15.0;
pub(super) const FONT_WEIGHT_NORMAL: FontWeight = FontWeight::NORMAL;
pub(super) const FONT_WEIGHT_STRONG: FontWeight = FontWeight::SEMIBOLD;

// ── Motion ─────────────────────────────────────────────────────────────────
/// Chrome reveal/conceal fade.
pub(super) const FADE_DURATION: Duration = Duration::from_millis(200);
/// Hover colour transition.
pub(super) const HOVER_DURATION: Duration = Duration::from_millis(110);

/// QML `Easing.OutCubic`.
pub(super) fn fade_out_cubic(delta: f32) -> f32 {
    1.0 - (1.0 - delta).powi(3)
}

// ── Bundled icons ──────────────────────────────────────────────────────────
// Same bytes as the QML resource module (`build.rs` registers the directory),
// including the Tabler attribution. Names are the SVG file stems; `path` is a
// stable identity keying the GPUI SVG sprite atlas, so an icon is parsed and
// rasterized once per (path, size) instead of per frame.

/// One bundled glyph: its stable asset path and embedded bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Icon {
    pub(super) path: &'static str,
    pub(super) bytes: &'static [u8],
}

const ICONS: [(&str, Icon); 15] = [
    (
        "adjustments",
        Icon {
            path: "icons/adjustments.svg",
            bytes: include_bytes!("../../../assets/icons/adjustments.svg"),
        },
    ),
    (
        "camera",
        Icon {
            path: "icons/camera.svg",
            bytes: include_bytes!("../../../assets/icons/camera.svg"),
        },
    ),
    (
        "check",
        Icon {
            path: "icons/check.svg",
            bytes: include_bytes!("../../../assets/icons/check.svg"),
        },
    ),
    (
        "chevron-down",
        Icon {
            path: "icons/chevron-down.svg",
            bytes: include_bytes!("../../../assets/icons/chevron-down.svg"),
        },
    ),
    (
        "headphones",
        Icon {
            path: "icons/headphones.svg",
            bytes: include_bytes!("../../../assets/icons/headphones.svg"),
        },
    ),
    (
        "maximize",
        Icon {
            path: "icons/maximize.svg",
            bytes: include_bytes!("../../../assets/icons/maximize.svg"),
        },
    ),
    (
        "minimize",
        Icon {
            path: "icons/minimize.svg",
            bytes: include_bytes!("../../../assets/icons/minimize.svg"),
        },
    ),
    (
        "palette",
        Icon {
            path: "icons/palette.svg",
            bytes: include_bytes!("../../../assets/icons/palette.svg"),
        },
    ),
    (
        "pause",
        Icon {
            path: "icons/pause.svg",
            bytes: include_bytes!("../../../assets/icons/pause.svg"),
        },
    ),
    (
        "play",
        Icon {
            path: "icons/play.svg",
            bytes: include_bytes!("../../../assets/icons/play.svg"),
        },
    ),
    (
        "refresh",
        Icon {
            path: "icons/refresh.svg",
            bytes: include_bytes!("../../../assets/icons/refresh.svg"),
        },
    ),
    (
        "settings",
        Icon {
            path: "icons/settings.svg",
            bytes: include_bytes!("../../../assets/icons/settings.svg"),
        },
    ),
    (
        "volume",
        Icon {
            path: "icons/volume.svg",
            bytes: include_bytes!("../../../assets/icons/volume.svg"),
        },
    ),
    (
        "volume-off",
        Icon {
            path: "icons/volume-off.svg",
            bytes: include_bytes!("../../../assets/icons/volume-off.svg"),
        },
    ),
    (
        "x",
        Icon {
            path: "icons/x.svg",
            bytes: include_bytes!("../../../assets/icons/x.svg"),
        },
    ),
];

/// The bundled SVG for `name`, or `None` for an unknown glyph.
pub(super) fn icon(name: &str) -> Option<Icon> {
    ICONS
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, icon)| *icon)
}

#[cfg(test)]
mod tests {
    use super::{CONTROL_PRESSED, FADE_DURATION, SURFACE0, fade_out_cubic, icon};

    #[test]
    fn icons_are_the_bundled_tabler_svgs() {
        let mut paths = std::collections::HashSet::new();
        for (name, icon) in super::ICONS {
            let text = std::str::from_utf8(icon.bytes).expect("icon is UTF-8");
            assert!(text.starts_with("<svg"), "{name} is not an SVG");
            assert!(text.contains("stroke="), "{name} has no stroked glyph");
            assert_eq!(icon.path, format!("icons/{name}.svg"));
            assert!(paths.insert(icon.path), "duplicate icon path {}", icon.path);
        }
        assert!(icon("pause").is_some());
        assert!(icon("play").is_some());
        assert!(icon("settings").is_some());
        assert!(icon("missing").is_none());
    }

    #[test]
    fn derived_tokens_follow_the_palette() {
        assert_ne!(CONTROL_PRESSED, SURFACE0);
        assert_eq!(fade_out_cubic(0.0), 0.0);
        assert_eq!(fade_out_cubic(1.0), 1.0);
        assert_eq!(FADE_DURATION.as_millis(), 200);
    }
}
