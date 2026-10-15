import QtQuick
import QtQuick.Controls
import dev.antho.furami

// Themed checkbox used for the category toggles (audio,
// deband, interpolation). A disabled control is the genuine off switch; the
// checked state only enables the staged domain value.
CheckBox {
    id: control
    property string label: ""
    text: control.label.replace(/&/g, "&&")
    Accessible.name: control.label

    focusPolicy: Qt.StrongFocus
    Keys.onPressed: function(event) { Chrome.noteKeyboardActivity() }
    font.family: Brume.fontFamily
    font.pixelSize: Brume.fontSize
    implicitHeight: Brume.controlHeight
    leftPadding: 0
    spacing: Brume.gap
    palette.windowText: enabled ? Brume.foreground : Brume.disabledText
    palette.text: enabled ? Brume.foreground : Brume.disabledText
    palette.highlight: Brume.accent
    palette.window: Brume.control

    onPressedChanged: Chrome.setInteractionHeld(control.pressed)

    indicator: Rectangle {
        implicitWidth: 20
        implicitHeight: 20
        x: control.leftPadding
        y: control.topPadding + (control.availableHeight - height) / 2
        radius: 5
        color: !control.enabled ? Brume.disabledFill
            : control.checked ? Brume.accent : Brume.track
        border.width: control.visualFocus ? 2 : 1
        border.color: control.visualFocus ? Brume.accent
            : control.checked ? Brume.accent : Brume.divider

        BrumeIcon {
            anchors.centerIn: parent
            iconSize: 14
            source: Brume.icon("check")
            tint: Brume.matte
            visible: control.checked
        }
    }

    contentItem: BrumeLabel {
        leftPadding: control.indicator.width + control.spacing
        text: control.label
        variant: control.enabled ? "body" : "secondary"
        verticalAlignment: Text.AlignVCenter
        wrapMode: Text.Wrap
    }
}
