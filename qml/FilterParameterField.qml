import QtQuick
import QtQuick.Layouts
import dev.antho.furami

// A parameter of one revision-scoped filter entry, never a renderer field.
ColumnLayout {
    id: field

    required property var row
    required property var value
    required property string revision
    required property string entryKey
    required property bool editorActive
    readonly property string fieldKey: field.row.key
    readonly property string fieldType: field.row.type
    property bool numberEditPending: false
    property bool editStale: false
    property bool touched: false
    property bool alive: true
    property string editRevision: ""
    property string editEntry: ""
    property string editParameter: ""
    property string refreshIdentity: ""
    readonly property string identity: field.revision + "|" + field.entryKey
        + "|" + field.fieldKey + "|" + field.fieldType

    signal numberRequested(string revision, string entry, string parameter, real value)
    signal choiceRequested(string revision, string entry, string parameter, string value)

    Layout.fillWidth: true
    spacing: Brume.gapSmall

    function identityMatches() {
        return field.editRevision === field.revision && field.editEntry === field.entryKey
            && field.editParameter === field.fieldKey
    }
    function invalidateEdit() {
        if (field.numberEditPending) {
            field.numberEditPending = false
            field.editStale = true
        }
        if (blurCompletion)
            blurCompletion.stop()
        field.scheduleRefresh()
    }
    onIdentityChanged: field.invalidateEdit()
    onFieldKeyChanged: field.replaceParameter()
    onFieldTypeChanged: field.replaceParameter()
    onEditorActiveChanged: {
        if (!field.editorActive) {
            field.invalidateEdit()
            if (projectionRefresh)
                projectionRefresh.stop()
        }
    }
    Component.onDestruction: {
        field.alive = false
        field.numberEditPending = false
        if (blurCompletion)
            blurCompletion.stop()
        if (projectionRefresh)
            projectionRefresh.stop()
    }

    function scheduleRefresh() {
        if (!projectionRefresh)
            return
        field.refreshIdentity = field.identity
        projectionRefresh.restart()
    }
    function replaceParameter() {
        // A different parameter/type must not inherit an obsolete numeric
        // buffer or an error recoverable only through a now-hidden input.
        field.numberEditPending = false
        field.editStale = false
        field.touched = false
        if (blurCompletion)
            blurCompletion.stop()
        field.scheduleRefresh()
    }
    Timer {
        id: projectionRefresh
        interval: 0
        onTriggered: {
            if (field.alive && field.editorActive && field.identity === field.refreshIdentity
                    && !field.numberEditPending && !field.editStale)
                numberField.reset()
        }
    }

    readonly property bool stepValid: field.fieldType !== "integer"
        || field.row.step === null
        || (numberField.numericValue - field.row.min) % field.row.step === 0
    readonly property string inputError: {
        if (field.editStale)
            return "La sélection a changé. Revenez dans ce champ pour recommencer."
        if (!field.touched || (numberField.valid && field.stepValid))
            return ""
        const range = String(field.row.min) + " à " + String(field.row.max)
        return field.fieldType === "integer"
            ? "Entrez un entier de " + range + " (pas de " + field.row.step + ")."
            : "Entrez un nombre fini de " + range + " (point décimal ; notation e acceptée)."
    }
    function commitNumber() {
        if (!field.alive || !field.editorActive || !field.enabled
                || !field.numberEditPending || field.editStale || !field.identityMatches()
                || !numberField.valid || !field.stepValid)
            return
        const value = numberField.value()
        if (!isFinite(value))
            return
        // Consume before dispatch: synchronous snapshots and Enter + blur cannot replay.
        field.numberEditPending = false
        blurCompletion.stop()
        field.numberRequested(field.editRevision, field.editEntry, field.editParameter, value)
    }

    // A pointer press on Back/Close may blur before its clicked handler closes
    // the editor. Defer only blur, then recheck the live lifecycle and identity.
    Timer {
        id: blurCompletion
        interval: 0
        onTriggered: field.commitNumber()
    }

    BrumeLabel {
        Layout.fillWidth: true
        text: field.row.label
        variant: "secondary"
    }
    BrumeChoice {
        objectName: "filterChoice_" + field.entryKey + "_" + field.fieldKey
        Layout.fillWidth: true
        visible: field.fieldType === "choice"
        rows: field.row.choices
        selectedKey: String(field.value)
        Accessible.name: field.row.label
        onActivated: {
            if (!field.alive || !field.editorActive || !field.enabled || currentIndex < 0)
                return
            field.choiceRequested(field.revision, field.entryKey, field.fieldKey,
                String(rows[currentIndex].key))
        }
    }
    BrumeNumberField {
        id: numberField
        objectName: "filterNumber_" + field.entryKey + "_" + field.fieldKey
        Layout.fillWidth: true
        visible: field.fieldType === "number" || field.fieldType === "integer"
        integerOnly: field.fieldType === "integer"
        scientificNotation: true
        minimum: field.row.min === null ? 0 : field.row.min
        maximum: field.row.max === null ? 0 : field.row.max
        projected: String(field.value)
        Accessible.name: field.row.label
        Accessible.description: field.inputError.length > 0 ? field.inputError : field.row.notice
        onTextEdited: {
            Chrome.noteKeyboardActivity()
            field.touched = true
            if (!field.editorActive || field.editStale)
                return
            if (!field.numberEditPending) {
                field.editRevision = field.revision
                field.editEntry = field.entryKey
                field.editParameter = field.fieldKey
                field.numberEditPending = true
            }
        }
        onAccepted: field.commitNumber()
        onActiveFocusChanged: {
            Chrome.setTextEntryActive(numberField.activeFocus)
            if (numberField.activeFocus) {
                blurCompletion.stop()
                if (field.editStale) {
                    numberField.reset()
                    field.editStale = false
                    field.touched = false
                }
            } else if (field.numberEditPending && field.editorActive) {
                blurCompletion.restart()
            }
        }
    }
    BrumeLabel {
        objectName: "filterInputError_" + field.entryKey + "_" + field.fieldKey
        Layout.fillWidth: true
        visible: text.length > 0
        variant: "error"
        text: field.inputError
    }
    BrumeLabel {
        Layout.fillWidth: true
        visible: text.length > 0
        variant: "note"
        text: field.row.notice
    }
}
