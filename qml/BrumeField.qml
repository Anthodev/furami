import QtQuick
import QtQuick.Controls
import dev.antho.furami

// Themed single-line text input. Free text in Furami is only ever a local
// buffer (numeric edits); it is never submitted to media without a typed
// conversion and explicit user action.
TextField {
    id: control

    // Last accepted domain text. While the local buffer is invalid it is kept
    // (never silently replaced); a valid unfocused buffer yields to the
    // projection again.
    property string projected: ""

    readonly property bool valid: control.acceptableInput && control.text.length > 0
    readonly property bool dirty: control.text !== control.projected

    focusPolicy: Qt.StrongFocus
    Keys.onPressed: function(event) { Chrome.noteKeyboardActivity() }
    font.family: Brume.fontFamily
    font.pixelSize: Brume.fontSize
    implicitHeight: Brume.controlHeight
    leftPadding: Brume.gap
    rightPadding: Brume.gap
    selectByMouse: true
    placeholderTextColor: Brume.disabledText
    palette.base: Brume.control
    palette.text: enabled ? Brume.foreground : Brume.disabledText
    palette.placeholderText: Brume.disabledText
    palette.highlight: Brume.accent
    palette.highlightedText: Brume.matte

    onActiveFocusChanged: Chrome.setTextEntryActive(activeFocus)
    onTextEdited: Chrome.noteKeyboardActivity()

    onProjectedChanged: {
        if (!control.valid)
            return
        if (control.activeFocus)
            return
        control.text = control.projected
    }
    Component.onCompleted: control.text = control.projected

    function reset() {
        control.text = control.projected
    }

    background: Rectangle {
        radius: Brume.radiusControl
        color: control.enabled ? Brume.control : Brume.disabledFill
        border.width: control.visualFocus ? 2 : 1
        border.color: control.visualFocus ? Brume.accent
            : control.text.length === 0 ? Brume.divider : Brume.separator
    }
}
