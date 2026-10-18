pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Capture source selector (immediate selection).
//
// Real device catalog rows only: no fabricated entries, no silent node
// substitution. A healthy row shows only the device name; ineligible nodes
// stay listed with their real reason, and the catalog revision is echoed back
// with the opaque row key.
Item {
    id: view

    implicitWidth: Brume.popupMaxWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property var catalog: profile.catalog

    function choose(rowKey) {
        if (!Projection.bridge)
            return
        Projection.bridge.selectCapture(profile.selectionRevision, catalog.revision, rowKey)
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
                text: "Capture source"
                variant: "secondary"
                Layout.fillWidth: true
            }
            BrumeIconButton {
                objectName: "sourceRefresh"
                label: "Refresh catalogs"
                icon.source: Brume.icon("refresh")
                enabled: !view.catalog.busy && !view.profile.closing
                onClicked: view.refresh()
            }
        }

        BrumeLabel {
            objectName: "sourceCatalogError"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "error"
            text: view.catalog.error
        }

        BrumeLabel {
            objectName: "sourceCapability"
            Layout.fillWidth: true
            visible: view.catalog.capabilityReason.length > 0
            variant: "note"
            text: "Capabilities: " + view.catalog.capabilityStatus
                + " — " + view.catalog.capabilityReason
        }

        BrumeScrollView {
            Layout.fillWidth: true
            Layout.preferredHeight: Math.min(sourceList.implicitHeight, Brume.popupMaxListHeight)
            clip: true
            contentWidth: availableWidth
            contentHeight: sourceList.implicitHeight
            ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
            ScrollBar.vertical.policy: ScrollBar.AsNeeded

            ColumnLayout {
                id: sourceList
                width: Math.max(0, parent.width)
                spacing: Brume.gapSmall
                enabled: view.profile.canEditSelection

                Repeater {
                    model: view.catalog.sourceRows

                    ChoiceRow {
                        required property var modelData
                        objectName: "sourceRow_" + modelData.key
                        Layout.fillWidth: true
                        // Same-device siblings can share a name: the opaque row
                        // key still routes the exact physical node, so no
                        // technical disambiguation label is added.
                        caption: String(modelData.label || "")
                        reason: String(modelData.reason || "")
                        showReason: false
                        eligible: modelData.eligible === true
                        selected: String(modelData.key) === view.profile.catalog.selectedSourceKey
                        onActivated: view.choose(String(modelData.key))
                    }
                }
            }
        }

        BrumeLabel {
            Layout.fillWidth: true
            visible: view.catalog.sourceRows.length === 0
            variant: "note"
            text: view.catalog.busy
                ? "Discovering sources…"
                : "No source reported by the catalog yet."
        }
    }
}
