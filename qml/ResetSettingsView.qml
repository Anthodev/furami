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
            text: "Remplacer le fichier de réglages ?"
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
                    text: "Le fichier d'origine sera remplacé et son contenu perdu :"
                }
                BrumeLabel {
                    Layout.fillWidth: true
                    variant: "note"
                    text: view.profile.settings.path
                }
                BrumeLabel {
                    Layout.fillWidth: true
                    text: "Tous les profils de source et la dernière source enregistrée seront effacés. Les préférences globales actuelles sont conservées ; la capture en cours n'est ni arrêtée ni modifiée."
                }
            }
        }

        BrumeButton {
            id: cancelButton
            objectName: "resetCancel"
            Layout.fillWidth: true
            label: "Annuler"
            Accessible.name: label
            onClicked: if (Projection.bridge)
                Projection.bridge.decideSettingsReset(view.expectedToken, false)
        }
        BrumeButton {
            id: confirmButton
            objectName: "resetConfirm"
            Layout.fillWidth: true
            label: "Remplacer le fichier d'origine"
            Accessible.name: label
            onClicked: if (Projection.bridge)
                Projection.bridge.decideSettingsReset(view.expectedToken, true)
        }
    }
}
