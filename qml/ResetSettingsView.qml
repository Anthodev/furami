import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Settings-file reset confirmation echoes the displayed token to the typed
// decision. Generic Escape/outside presses never silently cancel the request.
Item {
    id: view

    implicitWidth: Brume.settingsWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property string expectedToken: profile.settings.resetToken
    function focusBoundary(backward) {
        (backward ? confirmButton : cancelButton).forceActiveFocus(Qt.PopupFocusReason)
    }

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gap

        BrumeLabel {
            Layout.fillWidth: true
            text: "Replace the settings file?"
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
                    text: "The original file will be replaced and its content lost:"
                }
                BrumeLabel {
                    Layout.fillWidth: true
                    variant: "note"
                    text: view.profile.settings.path
                }
                BrumeLabel {
                    Layout.fillWidth: true
                    text: "Every source profile and the last saved source will be erased. Current global preferences are kept; the running capture is neither stopped nor changed."
                }
            }
        }

        BrumeButton {
            id: cancelButton
            objectName: "resetCancel"
            Layout.fillWidth: true
            label: "Cancel"
            Accessible.name: label
            onClicked: if (Projection.bridge)
                Projection.bridge.decideSettingsReset(view.expectedToken, false)
        }
        BrumeButton {
            id: confirmButton
            objectName: "resetConfirm"
            Layout.fillWidth: true
            label: "Replace the original file"
            Accessible.name: label
            onClicked: if (Projection.bridge)
                Projection.bridge.decideSettingsReset(view.expectedToken, true)
        }
    }
}
