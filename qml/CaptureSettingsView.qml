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

        // The section title lives in the shared menu header; the refresh
        // control stays right-aligned, preceded by the fill-width seam the
        // title used to provide.
        RowLayout {
            Layout.fillWidth: true
            Item { Layout.fillWidth: true }
            BrumeIconButton {
                objectName: "captureRefresh"
                label: "Refresh catalogs"
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
            emptyText: "Format unavailable"
            Accessible.name: "Input format"
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
            text: "Requested: " + (view.selection.requestedFormatLabel.length > 0
                    ? view.selection.requestedFormatLabel : "—")
                + "   Resolved: " + (view.selection.resolvedFormatLabel.length > 0
                    ? view.selection.resolvedFormatLabel : "—")
        }

        BrumeLabel {
            Layout.fillWidth: true
            variant: "note"
            text: "A previously selected mode that became incompatible stays listed as unavailable until an explicit choice."
        }
    }
}
