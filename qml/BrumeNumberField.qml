import QtQuick
import QtQuick.Controls
import dev.antho.furami

// Numeric editor with a preserved local buffer (FUR-029 contract).
//
// * `projected` is the last accepted domain value.
// * Invalid text stays visible while the field is edited or while unrelated
//   snapshot updates arrive; it never reaches media.
// * When the buffer is valid and unfocused, the projection wins again.
TextField {
    id: control

    property string projected: ""
    property bool integerOnly: true
    property real minimum: 0
    property real maximum: 1e9

    readonly property real numericValue: Number(control.text)
    readonly property bool valid: control.acceptableInput
        && (control.integerOnly ? /^[+-]?[0-9]+$/ : /^[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)$/).test(control.text)
        && isFinite(control.numericValue)
        && control.numericValue >= control.minimum && control.numericValue <= control.maximum
        && (!control.integerOnly || Math.floor(control.numericValue) === control.numericValue)
    readonly property bool dirty: control.text !== control.projected

    focusPolicy: Qt.StrongFocus
    Keys.onPressed: function(event) { Chrome.noteKeyboardActivity() }
    font.family: Brume.fontFamily
    font.pixelSize: Brume.fontSize
    implicitHeight: Brume.controlHeight
    implicitWidth: 88
    horizontalAlignment: TextInput.AlignRight
    leftPadding: Brume.gap
    rightPadding: Brume.gap
    selectByMouse: true
    placeholderTextColor: Brume.disabledText
    palette.base: Brume.control
    palette.text: enabled ? Brume.foreground : Brume.disabledText
    palette.placeholderText: Brume.disabledText
    palette.highlight: Brume.accent
    palette.highlightedText: Brume.matte
    inputMethodHints: Qt.ImhFormattedNumbersOnly
    validator: control.integerOnly
        ? integerValidator
        : doubleValidator

    RegularExpressionValidator {
        id: integerValidator
        regularExpression: /^[+-]?[0-9]+$/
    }
    DoubleValidator {
        id: doubleValidator
        locale: "C"
        bottom: control.minimum
        top: control.maximum
        notation: DoubleValidator.StandardNotation
    }

    onProjectedChanged: {
        if (!control.valid)
            return
        if (control.activeFocus)
            return
        control.text = control.projected
    }
    Component.onCompleted: control.text = control.projected

    onActiveFocusChanged: Chrome.setTextEntryActive(activeFocus)
    onTextEdited: Chrome.noteKeyboardActivity()

    function reset() {
        control.text = control.projected
    }

    // Parsed value of the current buffer, or NaN when it cannot be submitted.
    function value() {
        if (!control.valid)
            return NaN
        return control.numericValue
    }

    background: Rectangle {
        radius: Brume.radiusControl
        color: control.enabled ? Brume.control : Brume.disabledFill
        border.width: control.visualFocus ? 2 : 1
        border.color: control.visualFocus ? Brume.accent
            : control.valid ? Brume.divider : Brume.error
    }
}
