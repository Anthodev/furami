pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Capture settings: input format policy (immediate). Auto never falls back to
// RGB/unknown; manual rows are the real formats eligible for the selected
// node.
Item {
    id: view

    implicitWidth: Brume.settingsWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property var catalog: profile.catalog
    readonly property var selection: profile.selection

    function chooseFormat(rowKey) {
        if (!Projection.bridge)
            return
        Projection.bridge.selectFormat(profile.selectionRevision, catalog.revision, rowKey)
    }
    function refresh() {
        if (Projection.bridge)
            Projection.bridge.refreshCatalogs()
    }

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gap

        RowLayout {
            Layout.fillWidth: true
            BrumeLabel {
                Layout.fillWidth: true
                text: "Capture — format d'entrée"
                variant: "title"
            }
            BrumeIconButton {
                objectName: "captureRefresh"
                label: "Actualiser les catalogues"
                icon.source: Brume.icon("refresh")
                enabled: !view.catalog.busy && !view.profile.closing
                onClicked: view.refresh()
            }
        }

        BrumeLabel {
            objectName: "captureFormatError"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "error"
            text: view.catalog.error
        }

        BrumeChoice {
            objectName: "captureFormatSelector"
            refreshCatalogOnOpen: true
            Layout.fillWidth: true
            enabled: view.profile.canEditSelection && view.selection.present
            rows: view.catalog.formatRows
            selectedKey: view.selection.formatPolicy === "manual" ? view.selection.manualFormatKey : "auto"
            emptyText: "Format indisponible"
            Accessible.name: "Format d'entrée"
            onActivated: {
                if (currentIndex < 0)
                    return
                view.chooseFormat(String(rows[currentIndex].key))
            }
        }

        BrumeLabel {
            objectName: "captureFormatResolved"
            Layout.fillWidth: true
            variant: "note"
            text: "Demandé : " + (view.selection.requestedFormatLabel.length > 0
                    ? view.selection.requestedFormatLabel : "—")
                + "   Résolu : " + (view.selection.resolvedFormatLabel.length > 0
                    ? view.selection.resolvedFormatLabel : "—")
        }

        ColumnLayout {
            id: formatList
            Layout.fillWidth: true
            spacing: Brume.gapSmall
            enabled: view.profile.canEditSelection && view.selection.present

            Repeater {
                model: view.catalog.formatRows

                ChoiceRow {
                    required property var modelData
                    Layout.fillWidth: true
                    caption: String(modelData.label || "")
                    // The FourCC equals the label in the catalog projection: an
                    // identical value is never shown twice.
                    detail: String(modelData.fourcc || "") === String(modelData.label || "")
                        ? "" : String(modelData.fourcc || "")
                    reason: String(modelData.reason || "")
                    eligible: modelData.eligible === true
                    selected: String(modelData.key)
                        === (view.selection.formatPolicy === "manual" ? view.selection.manualFormatKey : "auto")
                    onActivated: view.chooseFormat(String(modelData.key))
                }
            }
        }

        BrumeLabel {
            Layout.fillWidth: true
            variant: "note"
            text: "Un ancien mode devenu incompatible reste affiché comme indisponible jusqu'à un choix explicite."
        }
    }
}
