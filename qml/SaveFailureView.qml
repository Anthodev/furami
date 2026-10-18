import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Save-failure / durability-warning confirmation. The existing close tokens are
// owned by the runtime; the view only offers retry or an explicit close.
Item {
    id: view

    implicitWidth: Brume.settingsWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property bool warning: profile.settings.closeDialog === "warning"
    function focusBoundary(backward) {
        (backward || view.warning ? closeButton : retryButton).forceActiveFocus(Qt.PopupFocusReason)
    }

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gap

        BrumeLabel {
            Layout.fillWidth: true
            text: view.warning
                ? "Durability not confirmed"
                : "Save failed"
            variant: "title"
        }

        ScrollView {
            Layout.fillWidth: true
            Layout.preferredHeight: Math.min(message.implicitHeight, Brume.popupMaxListHeight)
            clip: true
            contentWidth: availableWidth
            contentHeight: message.implicitHeight
            ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
            ScrollBar.vertical.policy: ScrollBar.AsNeeded

            ColumnLayout {
                id: message
                width: Math.max(0, parent.width)
                spacing: Brume.gapSmall
                BrumeLabel {
                    Layout.fillWidth: true
                    text: "Media release is complete; no media owner will reopen."
                }
                BrumeLabel {
                    Layout.fillWidth: true
                    variant: "note"
                    text: view.profile.settings.status
                }
                BrumeLabel {
                    Layout.fillWidth: true
                    visible: Projection.bridge && String(Projection.bridge.diagnostic || "").length > 0
                    variant: "note"
                    text: Projection.bridge ? String(Projection.bridge.diagnostic || "") : ""
                }
            }
        }

        BrumeButton {
            id: retryButton
            objectName: "saveFailureRetry"
            Layout.fillWidth: true
            visible: !view.warning
            label: "Retry save"
            Accessible.name: label
            onClicked: if (Projection.bridge)
                Projection.bridge.retrySave()
        }
        BrumeButton {
            id: closeButton
            objectName: "saveFailureClose"
            Layout.fillWidth: true
            label: view.warning ? "Close without confirmed durability" : "Close without saving"
            Accessible.name: label
            onClicked: if (Projection.bridge)
                Projection.bridge.closeWithoutSave()
        }
    }
}
