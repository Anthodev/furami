import QtQuick
import QtQuick.Effects
import dev.antho.furami

// Bundled monochrome SVG icon, tinted with an approved Brume colour.
//
// The shipped SVGs are white strokes, so the effect chain is exact:
// brightness lifts the source to white and colorization multiplies it by the
// requested tint, preserving the alpha mask. This is used for decorative
// glyphs; buttons keep the Qt Controls icon path, which tints with the same
// CompositionMode_SourceIn semantics.
Item {
    id: icon

    property url source: ""
    property color tint: Brume.foreground
    property int iconSize: Brume.iconSize

    implicitWidth: icon.iconSize
    implicitHeight: icon.iconSize

    Image {
        id: base
        anchors.fill: parent
        source: icon.source
        sourceSize: Qt.size(icon.iconSize * 2, icon.iconSize * 2)
        fillMode: Image.PreserveAspectFit
        visible: false
    }

    MultiEffect {
        anchors.fill: parent
        source: base
        brightness: 1.0
        colorization: 1.0
        colorizationColor: icon.tint
        visible: icon.visible
        Accessible.ignored: true
    }
}
