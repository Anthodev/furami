import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Themed Brume button: sentence-case, icon tinted with the approved palette,
// visible keyboard focus ring, and a real press/drag hold for the idle logic.
Button {
    id: control
    property string label: ""
    // AbstractButton itself registers mnemonic shortcuts from text, even with
    // a custom contentItem. Escape that transport; draw the literal label.
    text: control.label.replace(/&/g, "&&")

    focusPolicy: Qt.StrongFocus
    Keys.onPressed: function(event) { Chrome.noteKeyboardActivity() }
    font.family: Brume.fontFamily
    font.pixelSize: Brume.fontSize
    font.capitalization: Font.MixedCase
    leftPadding: Brume.edge
    rightPadding: Brume.edge
    topPadding: 6
    bottomPadding: 6
    spacing: Brume.gapSmall
    implicitHeight: Brume.controlHeight
    icon.width: Brume.iconSize
    icon.height: Brume.iconSize
    icon.color: control.enabled
        ? (control.checked || control.highlighted ? Brume.accent : Brume.foreground)
        : Brume.disabledText
    Accessible.name: control.label

    // Qt's style IconLabel interprets ampersands as mnemonics. Catalog and
    // loaded labels are external data: display their exact plain-text identity.
    contentItem: RowLayout {
        spacing: control.spacing
        BrumeIcon {
            visible: control.icon.source.toString().length > 0
                && control.display !== AbstractButton.TextOnly
            source: control.icon.source
            iconSize: control.icon.width
            tint: control.icon.color
            Layout.alignment: Qt.AlignVCenter
        }
        BrumeLabel {
            Layout.fillWidth: true
            visible: control.display !== AbstractButton.IconOnly
            text: control.label
            font: control.font
            color: control.enabled ? Brume.foreground : Brume.disabledText
            horizontalAlignment: Text.AlignHCenter
            verticalAlignment: Text.AlignVCenter
            wrapMode: Text.NoWrap
            elide: Text.ElideRight
        }
    }

    palette.buttonText: control.enabled ? Brume.foreground : Brume.disabledText
    palette.windowText: control.enabled ? Brume.foreground : Brume.disabledText
    palette.highlight: Brume.accent
    palette.brightText: Brume.accent
    palette.button: Brume.control
    palette.mid: Brume.controlHover
    palette.dark: Brume.controlPressed

    onPressedChanged: Chrome.setInteractionHeld(control.pressed)

    background: Rectangle {
        radius: Brume.radiusControl
        color: !control.enabled ? Brume.disabledFill
            : control.down ? Brume.controlPressed
            : control.hovered ? Brume.controlHover
            : Brume.control
        border.width: control.visualFocus ? 2 : 1
        border.color: control.visualFocus ? Brume.accent : Brume.divider
        Behavior on color {
            ColorAnimation { duration: Brume.hoverDuration }
        }
    }
}
