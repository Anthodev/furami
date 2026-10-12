pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Resolution / framerate selector (immediate selection).
//
// Discrete rows are genuine descriptors of the selected node; the exact
// rational framerate stays unrounded. When the runtime reports stepwise or
// continuous descriptors for the current policy, every exact per-format range
// is shown with typed width/height (or N/D) editors instead of a sampled
// invented list. A finished, valid entry commits once; an invalid buffer stays
// readable and never reaches the media path.
Item {
    id: view

    // "size" | "rate"
    property string kind: "size"

    implicitWidth: Brume.popupMaxWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property var catalog: profile.catalog
    readonly property bool sizeMode: view.kind === "size"
    readonly property bool editable: profile.canEditSelection && profile.selection.present
    readonly property var rows: view.sizeMode ? profile.catalog.sizeRows : profile.catalog.rateRows
    readonly property var ranges: {
        const list = view.sizeMode ? catalog.sizeRanges : catalog.intervalRanges
        // The producer already filters by the policy and runtime support. Auto
        // retains each real format's provenance; it has no "auto" descriptor.
        if (profile.selection.formatPolicy !== "manual")
            return list
        const selected = []
        for (let i = 0; i < list.length; ++i) {
            if (String(list[i].formatKey) === profile.selection.manualFormatKey)
                selected.push(list[i])
        }
        return selected
    }

    // One pending-user-edit flag per editor. It is set only by a genuine user
    // edit (TextField.textEdited) and consumed by the first finished valid
    // completion, before any dispatch. Programmatic projection updates never
    // re-arm it, so a canonicalized acknowledgement or a refused value cannot
    // replay on the following blur, while later genuine typing always re-arms.
    property bool sizeEditPending: false
    property bool rateEditPending: false

    // Exact `N` or `N/D` unsigned decimal, positive u32 components, denominator
    // implicit 1. Mirrors FrameRate::parse_exact with lexical bounds only, so no
    // float conversion can round an oversized or zero component into validity.
    function u32DecimalPositive(digits) {
        if (typeof digits !== "string" || digits.length === 0 || !/^[0-9]+$/.test(digits))
            return false
        const significant = digits.replace(/^0+/, "")
        if (significant.length === 0)
            return false
        const maximum = "4294967295"
        return significant.length < maximum.length
            || (significant.length === maximum.length && significant <= maximum)
    }
    function exactRateValid(text) {
        if (typeof text !== "string")
            return false
        const parts = text.split("/")
        if (parts.length > 2)
            return false
        const denominator = parts.length === 2 ? parts[1] : "1"
        return view.u32DecimalPositive(parts[0]) && view.u32DecimalPositive(denominator)
    }

    function choose(rowKey) {
        if (!Projection.bridge)
            return
        if (view.sizeMode)
            Projection.bridge.selectSize(profile.selectionRevision, catalog.revision, rowKey)
        else
            Projection.bridge.selectRate(profile.selectionRevision, catalog.revision, rowKey)
    }

    // One completion for a finished, valid entry that follows a genuine user
    // edit. Width and height must form a complete pair; the rate must be a
    // valid exact rational. The pending flag is consumed before dispatch, so
    // Enter followed by focus loss cannot submit twice, and an invalid buffer
    // stays readable with no media intent.
    function commitEditors() {
        if (!Projection.bridge)
            return
        if (view.sizeMode) {
            if (!view.sizeEditPending || !widthField.valid || !heightField.valid)
                return
            view.sizeEditPending = false
            Projection.bridge.setSize(profile.selectionRevision, catalog.revision,
                widthField.text, heightField.text)
        } else {
            if (!view.rateEditPending || !rateField.valid || !view.exactRateValid(rateField.text))
                return
            view.rateEditPending = false
            Projection.bridge.setRate(profile.selectionRevision, catalog.revision, rateField.text)
        }
    }

    // A row detail only repeats the numeric value when it differs from the
    // caption: identical numbers are never shown twice.
    function rowDetail(modelData) {
        const extra = view.sizeMode && modelData.width !== undefined
            ? String(modelData.width) + " × " + String(modelData.height)
            : String(modelData.exact || "")
        return extra === String(modelData.label || "") ? "" : extra
    }

    function rangeText(value) {
        if (!value)
            return "—"
        if (typeof value === "object" && value.numerator !== undefined)
            return String(value.numerator) + "/" + String(value.denominator)
        if (typeof value === "object" && value.width !== undefined)
            return String(value.width) + " × " + String(value.height)
        return String(value)
    }

    function rangeFormatText(formatKey) {
        for (let i = 0; i < catalog.formatRows.length; ++i) {
            const row = catalog.formatRows[i]
            if (String(row.key) === String(formatKey))
                return String(row.label)
        }
        return String(formatKey)
    }

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gap

        BrumeLabel {
            text: view.sizeMode ? "Résolution" : "Framerate"
            variant: "title"
            Layout.fillWidth: true
        }

        BrumeLabel {
            objectName: "sizeRateCatalogError"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "error"
            text: view.catalog.error
        }

        BrumeScrollView {
            Layout.fillWidth: true
            Layout.preferredHeight: Math.min(discreteList.implicitHeight, Brume.popupMaxListHeight)
            clip: true
            contentWidth: availableWidth
            contentHeight: discreteList.implicitHeight
            ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
            ScrollBar.vertical.policy: ScrollBar.AsNeeded

            ColumnLayout {
                id: discreteList
                width: Math.max(0, parent.width)
                spacing: Brume.gapSmall
                enabled: view.editable

                Repeater {
                    model: view.rows

                    ChoiceRow {
                        required property var modelData
                        objectName: (view.sizeMode ? "sizeRow_" : "rateRow_") + modelData.key
                        Layout.fillWidth: true
                        // Rate rows keep the exact rational in `exact`/`label`
                        // for selection and the accessible detail; the visible
                        // caption is the human "60 FPS" form.
                        caption: view.sizeMode
                            ? String(modelData.label || "")
                            : Projection.rateFps(String(modelData.exact || ""))
                        detail: view.rowDetail(modelData)
                        reason: String(modelData.reason || "")
                        eligible: modelData.eligible === true
                        selected: view.sizeMode
                            ? (view.profile.selection.size !== null
                                && String(modelData.width) === String(view.profile.selection.size.width)
                                && String(modelData.height) === String(view.profile.selection.size.height))
                            : (view.profile.selection.rate !== null
                                && String(modelData.exact) === Projection.rateText(view.profile.selection))
                        onActivated: view.choose(String(modelData.key))
                    }
                }
            }
        }

        BrumeLabel {
            Layout.fillWidth: true
            visible: view.rows.length === 0 && view.ranges.length === 0
            variant: "note"
            text: view.catalog.busy
                ? "Lecture des formats en cours…"
                : (view.catalog.capabilityReason
                    || "Aucun mode rapporté pour le nœud sélectionné.")
        }

        // Edit the exact tuple, without selecting a format or resolving Auto here.
        ColumnLayout {
            Layout.fillWidth: true
            visible: view.ranges.length > 0
            spacing: Brume.gapSmall
            enabled: view.editable

            Repeater {
                model: view.ranges

                BrumeLabel {
                    required property var modelData
                    objectName: (view.sizeMode ? "sizeRange_" : "rateRange_")
                        + modelData.formatKey
                    Layout.fillWidth: true
                    variant: "secondary"
                    text: view.rangeFormatText(modelData.formatKey) + " · "
                        + (view.sizeMode
                            ? (modelData.kind === "continuous" ? "Plage continue : " : "Plage : ")
                                + view.rangeText(modelData.min)
                                + " — " + view.rangeText(modelData.max)
                                + (modelData.stepWidth !== null
                                    && modelData.stepWidth !== undefined
                                    ? "   pas " + String(modelData.stepWidth) + " × "
                                        + String(modelData.stepHeight) : "")
                            : (modelData.kind === "continuous"
                                ? "Intervalle continu natif (période) : "
                                : "Intervalle natif (période) : ")
                                + view.rangeText(modelData.min)
                                + " — " + view.rangeText(modelData.max)
                                + (modelData.step
                                    ? "   pas " + view.rangeText(modelData.step) : ""))
                }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: Brume.gapSmall
                visible: view.sizeMode

                BrumeNumberField {
                    id: widthField
                    objectName: "sizeWidthField"
                    Layout.fillWidth: true
                    integerOnly: true
                    minimum: 1
                    maximum: 4294967295
                    projected: view.profile.selection.size
                        ? String(view.profile.selection.size.width) : ""
                    Accessible.name: "Largeur en pixels"
                    // A genuine user edit re-arms the pair; the inherited
                    // keyboard-activity hook is preserved explicitly.
                    onTextEdited: {
                        view.sizeEditPending = true
                        Chrome.noteKeyboardActivity()
                    }
                    onAccepted: view.commitEditors()
                    onActiveFocusChanged: {
                        if (!widthField.activeFocus && !heightField.activeFocus)
                            view.commitEditors()
                    }
                }
                BrumeNumberField {
                    id: heightField
                    objectName: "sizeHeightField"
                    Layout.fillWidth: true
                    integerOnly: true
                    minimum: 1
                    maximum: 4294967295
                    projected: view.profile.selection.size
                        ? String(view.profile.selection.size.height) : ""
                    Accessible.name: "Hauteur en pixels"
                    onTextEdited: {
                        view.sizeEditPending = true
                        Chrome.noteKeyboardActivity()
                    }
                    onAccepted: view.commitEditors()
                    onActiveFocusChanged: {
                        if (!widthField.activeFocus && !heightField.activeFocus)
                            view.commitEditors()
                    }
                }
            }

            BrumeField {
                id: rateField
                objectName: "rateExactField"
                Layout.fillWidth: true
                visible: !view.sizeMode
                validator: RegularExpressionValidator { regularExpression: /^[0-9]+(\/[0-9]+)?$/ }
                placeholderText: "30000/1001"
                projected: Projection.rateText(view.profile.selection)
                Accessible.name: "Framerate exact : un entier ou N/D"
                onTextEdited: {
                    view.rateEditPending = true
                    Chrome.noteKeyboardActivity()
                }
                onAccepted: view.commitEditors()
                onActiveFocusChanged: {
                    if (!rateField.activeFocus)
                        view.commitEditors()
                }
            }
        }

        BrumeLabel {
            Layout.fillWidth: true
            variant: "note"
            text: view.sizeMode
                ? "Une valeur hors descripteur est refusée par la validation exacte du moteur, jamais arrondie."
                : "Les lignes discrètes sont des cadences exactes ; la plage native est une période en secondes, jamais convertie en image/s. Une cadence hors descripteur est refusée, jamais arrondie."
        }
    }
}
