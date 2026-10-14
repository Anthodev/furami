import QtQuick
import QtQuick.Controls
import dev.antho.furami

// Icon-only Brume button for the chrome. Keeps a 48 px mouse hit target while
// the drawn affordance stays compact, with a tooltip and an accessible name.
Button {
    id: control

    // Accessible name and tooltip text. Never a device/OS string.
    property string label: ""

    focusPolicy: Qt.StrongFocus
    Keys.onPressed: function(event) { Chrome.noteKeyboardActivity() }
    implicitWidth: Brume.hitTarget
    implicitHeight: Brume.hitTarget
    padding: 0
    icon.width: Brume.ribbonIconSize
    icon.height: Brume.ribbonIconSize
    icon.color: control.enabled
        ? (control.checked ? Brume.accent : (control.hovered ? Brume.foreground : Brume.secondary))
        : Brume.disabledText

    Accessible.name: control.label
    Accessible.description: control.label
    ToolTip.visible: control.hovered && control.label.length > 0
    ToolTip.text: control.label
    ToolTip.delay: 600

    onPressedChanged: Chrome.setInteractionHeld(control.pressed)

    background: Rectangle {
        radius: Brume.radiusControl
        color: !control.enabled ? "transparent"
            : control.down ? Brume.controlPressed
            : control.hovered ? Brume.controlHover
            : control.checked ? Brume.control : "transparent"
        border.width: control.visualFocus ? 2 : 0
        border.color: Brume.accent
    }
}
