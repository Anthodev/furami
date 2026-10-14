pragma Singleton

import QtQuick

// FUR-029 design tokens.
//
// Colors: Catppuccin Macchiato (MIT, © Catppuccin) — https://catppuccin.com/palette/
//
// The palette swatches below are the only source of colour truth. Every other
// colour is derived from them so the palette cannot drift into a second theme.
// The role aliases (matte/surface/foreground/…) keep the stable names the QML
// controls already consume. Prune and Craie remain future preferences only and
// are deliberately not implemented here.
QtObject {
    // ── Catppuccin Macchiato swatches ────────────────────────────────────
    // (crust = deepest pane; subtext0 = dimmer secondary; overlay0/overlay2 =
    //  decorative/subtle — not needed by any surface today, so not declared.)
    readonly property color base: "#24273a"
    readonly property color mantle: "#1e2030"
    readonly property color surface0: "#363a4f"
    readonly property color surface1: "#494d64"
    readonly property color surface2: "#5b6078"
    readonly property color overlay1: "#8087a2"
    readonly property color text: "#cad3f5"
    readonly property color subtext1: "#b8c0e0"
    readonly property color blue: "#8aadf4"
    readonly property color red: "#ed8796"

    // ── Role aliases (stable names consumed by the controls) ─────────────
    // window & menus = base (official background pane); ribbon & surfaces =
    // surface0; controls = surface0 with hover surface1; borders = surface2;
    // body = text; secondary = subtext1; accent = blue. `matte` is also the
    // ink drawn on an accent fill (Catppuccin On Accent).
    readonly property color matte: base
    readonly property color surface: surface0
    readonly property color foreground: text
    readonly property color secondary: subtext1
    readonly property color accent: blue
    readonly property color separator: surface2

    // ── Semantic status (Catppuccin) ─────────────────────────────────────
    readonly property color error: red

    // ── Derived surfaces ─────────────────────────────────────────────────
    readonly property color window: matte
    readonly property color control: surface0
    readonly property color controlHover: surface1
    readonly property color controlPressed: Qt.darker(surface0, 1.12)
    readonly property color menuSurface: base
    readonly property color scrim: Qt.rgba(matte.r, matte.g, matte.b, 0.78)
    readonly property color track: mantle
    readonly property color trackFill: accent
    readonly property color disabledFill: controlPressed
    readonly property color disabledText: overlay1
    readonly property color divider: Qt.rgba(separator.r, separator.g, separator.b, 0.75)
    readonly property color shadow: Qt.rgba(0, 0, 0, 0.28)

    // ── Geometry ─────────────────────────────────────────────────────────
    readonly property int radiusSurface: 10
    readonly property int radiusControl: 7
    readonly property int gapSmall: 4
    readonly property int gap: 8
    readonly property int gapLarge: 16
    readonly property int edge: 10
    readonly property int windowMargin: 8
    readonly property int separatorWidth: 1
    readonly property int dividerWidth: 1

    // ── Controls ─────────────────────────────────────────────────────────
    readonly property int controlHeight: 36
    readonly property int hitTarget: 48
    readonly property int iconSize: 18
    readonly property int ribbonIconSize: 20
    readonly property int ribbonHeight: 48
    readonly property int volumeWidth: 44
    readonly property int popupMaxWidth: 420
    readonly property int menuWidth: 320
    readonly property int settingsWidth: 400
    readonly property int popupMaxListHeight: 320

    // ── Type ─────────────────────────────────────────────────────────────
    readonly property string fontFamily: "Sans Serif"
    readonly property int fontSize: 13
    readonly property int fontSizeSmall: 11
    readonly property int fontSizeTitle: 15
    readonly property int fontWeightNormal: Font.Normal
    readonly property int fontWeightStrong: Font.DemiBold

    // ── Motion ───────────────────────────────────────────────────────────
    readonly property int fadeDuration: 200
    readonly property int fadeEasing: Easing.OutCubic
    readonly property int hoverDuration: 110

    // ── Bundled icons ────────────────────────────────────────────────────
    readonly property string iconRoot: "qrc:/qt/qml/dev/antho/furami/qml/icons/"

    function icon(name) {
        return iconRoot + name + ".svg"
    }
}
