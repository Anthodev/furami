import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// One typed renderer field of the existing closed RendererSettings DTO.
//
// The control shape comes from the projected field type; the value is read
// from the selection renderer and every complete change is reported to the
// owning view, which maps it to the exact named method.
ColumnLayout {
    id: field

    required property var row
    required property var value
    readonly property string fieldKey: field.row ? field.row.key : ""
    readonly property string fieldType: field.row ? field.row.type : ""
    readonly property string fieldLabel: field.row ? field.row.label : ""
    readonly property var choices: field.row ? field.row.choices : []

    signal setRequested(string fieldKey, var newValue)

    Layout.fillWidth: true
    spacing: Brume.gapSmall

    BrumeLabel {
        Layout.fillWidth: true
        text: field.fieldLabel
        variant: "secondary"
    }

    BrumeChoice {
        objectName: "renderChoice_" + field.fieldKey
        Layout.fillWidth: true
        visible: field.fieldType === "choice"
        rows: field.choices
        selectedKey: field.value !== undefined && field.value !== null ? String(field.value) : ""
        enabled: field.choices.length > 1
        Accessible.name: field.fieldLabel
        onActivated: {
            if (currentIndex < 0)
                return
            field.setRequested(field.fieldKey, rows[currentIndex].key)
        }
    }

    BrumeToggle {
        id: booleanToggle
        objectName: "renderToggle_" + field.fieldKey
        Layout.fillWidth: true
        visible: field.fieldType === "boolean"
        label: field.value === true ? "Activé" : "Désactivé"
        checked: field.value === true
        Accessible.name: field.fieldLabel
        onClicked: field.setRequested(field.fieldKey, booleanToggle.checked)
    }

    // Pending genuine user edit for this numeric field. It is set only by
    // TextField.textEdited and consumed by the first finished valid completion,
    // before dispatch: programmatic projection updates never re-arm it, so a
    // canonicalized acknowledgement or a refused value cannot replay on the
    // following blur, while later typing always re-arms.
    property bool numberEditPending: false
    function commitNumber() {
        if (!field.numberEditPending || !numberField.valid)
            return
        const value = numberField.value()
        if (!isFinite(value))
            return
        field.numberEditPending = false
        field.setRequested(field.fieldKey, value)
    }

    RowLayout {
        Layout.fillWidth: true
        visible: field.fieldType === "number" || field.fieldType === "integer"
        spacing: Brume.gapSmall

        BrumeNumberField {
            id: numberField
            objectName: "renderValue_" + field.fieldKey
            Layout.fillWidth: true
            integerOnly: field.fieldType === "integer"
            minimum: field.row && field.row.min !== null ? field.row.min : 0
            maximum: field.row && field.row.max !== null ? field.row.max : 0
            projected: field.value !== undefined && field.value !== null ? String(field.value) : ""
            Accessible.name: field.fieldLabel
            // A genuine user edit re-arms the completion; the inherited
            // keyboard-activity hook is preserved explicitly.
            onTextEdited: {
                field.numberEditPending = true
                Chrome.noteKeyboardActivity()
            }
            onAccepted: field.commitNumber()
            onActiveFocusChanged: {
                if (!numberField.activeFocus)
                    field.commitNumber()
            }
        }
    }

    BrumeLabel {
        Layout.fillWidth: true
        visible: text.length > 0
        variant: "note"
        text: field.row ? field.row.notice : ""
    }
}
