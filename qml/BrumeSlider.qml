import QtQuick
import QtQuick.Controls
import dev.antho.furami

// Themed slider. Pressing or dragging is a real interaction hold for the idle
// logic; stationary hover alone never holds.
Slider {
    id: control

    focusPolicy: Qt.StrongFocus
    Keys.onPressed: function(event) { Chrome.noteKeyboardActivity() }
    palette.highlight: Brume.accent
    palette.windowText: Brume.foreground
    palette.text: Brume.foreground

    onPressedChanged: Chrome.setInteractionHeld(control.pressed)

    handle: Rectangle {
        x: control.leftPadding + (control.horizontal
            ? control.visualPosition * (control.availableWidth - width)
            : (control.availableWidth - width) / 2)
        y: control.topPadding + (control.horizontal
            ? (control.availableHeight - height) / 2
            : control.visualPosition * (control.availableHeight - height))
        implicitWidth: 18
        implicitHeight: 18
        radius: width / 2
        color: control.enabled ? Brume.foreground : Brume.disabledText
        border.width: control.visualFocus ? 2 : 0
        border.color: Brume.accent
    }

    background: Rectangle {
        x: control.horizontal ? control.leftPadding : (control.width - width) / 2
        y: control.horizontal ? (control.height - height) / 2 : control.topPadding
        implicitWidth: control.horizontal ? 200 : 6
        implicitHeight: control.horizontal ? 6 : 200
        width: control.horizontal ? control.availableWidth : implicitWidth
        height: control.horizontal ? implicitHeight : control.availableHeight
        radius: 3
        color: Brume.track

        Rectangle {
            width: control.horizontal ? control.visualPosition * parent.width : parent.width
            height: control.horizontal ? parent.height
                : (1 - control.visualPosition) * parent.height
            y: control.horizontal ? 0 : control.visualPosition * parent.height
            radius: 3
            color: control.enabled ? Brume.trackFill : Brume.disabledText
        }
    }
}
