pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Playback audio output selector (immediate preference).
//
// Reselecting a lost manual output re-sends the opaque row key of the observed
// row; it never restarts capture and never maps a missing manual choice onto
// Auto.
Item {
    id: view

    implicitWidth: Brume.popupMaxWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property var output: profile.output

    function choose(rowKey) {
        if (!Projection.bridge)
            return
        Projection.bridge.selectOutput(rowKey, output.revision)
    }

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gap

        BrumeLabel {
            text: "Audio output"
            variant: "secondary"
            Layout.fillWidth: true
        }

        BrumeScrollView {
            Layout.fillWidth: true
            Layout.preferredHeight: Math.min(outputList.implicitHeight, Brume.popupMaxListHeight)
            clip: true
            contentWidth: availableWidth
            contentHeight: outputList.implicitHeight
            ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
            ScrollBar.vertical.policy: ScrollBar.AsNeeded

            ColumnLayout {
                id: outputList
                width: Math.max(0, parent.width)
                spacing: Brume.gapSmall

                Repeater {
                    model: view.output.rows

                    ChoiceRow {
                        required property var modelData
                        objectName: "outputRow_" + modelData.key
                        Layout.fillWidth: true
                        // The opaque row key routes the exact sink; the row
                        // itself shows only the device name.
                        caption: String(modelData.label || "")
                        reason: String(modelData.reason || "")
                        showReason: false
                        eligible: modelData.eligible === true
                        selected: String(modelData.key) === view.profile.output.selectedKey
                        onActivated: view.choose(String(modelData.key))
                    }
                }
            }
        }

        BrumeLabel {
            objectName: "outputMissing"
            Layout.fillWidth: true
            visible: view.output.selectedKey.length === 0 && view.output.selectedLabel.length > 0
            variant: "error"
            text: "Selected output \"" + view.output.selectedLabel + "\" not found: no automatic substitution."
        }

        BrumeLabel {
            objectName: "outputStatus"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "note"
            text: view.output.status
                + (view.output.effectiveLabel.length > 0
                    ? "\nActive output: " + view.output.effectiveLabel : "")
        }
    }
}
