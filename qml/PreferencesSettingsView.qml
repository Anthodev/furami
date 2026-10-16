import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Preferences: immediate playback preferences and the real saved-file reset
// intent (the confirmation itself is an existing token-checked dialog).
Item {
    id: view

    implicitWidth: Brume.settingsWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property var settings: profile.settings

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gap

        BrumeLabel {
            Layout.fillWidth: true
            text: "Préférences"
            variant: "title"
        }

        BrumeLabel {
            objectName: "preferencesStatus"
            Layout.fillWidth: true
            variant: "note"
            text: view.settings.status
        }

        BrumeLabel {
            objectName: "preferencesStartup"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "note"
            text: view.settings.startupReason
        }

        BrumeLabel {
            objectName: "preferencesPath"
            Layout.fillWidth: true
            visible: view.settings.path.length > 0
            variant: "note"
            text: view.settings.path
        }

        BrumeLabel {
            objectName: "preferencesImmediate"
            Layout.fillWidth: true
            variant: "secondary"
            text: "Immédiat : volume " + view.profile.live.volumePercent + " %"
                + " · sourdine " + (view.profile.live.muted ? "activée" : "désactivée")
                + " · fenêtre " + (Projection.bridge && Projection.bridge.fullscreenPreference
                    ? "plein écran" : "fenêtrée")
        }

        BrumeToggle {
            objectName: "preferencesStatsHidden"
            Layout.fillWidth: true
            label: "Masquer les statistiques du flux"
            checked: Projection.bridge
                && Projection.bridge.statsHiddenPreference
            enabled: Projection.ready && !view.profile.closing
            Accessible.name: "Masquer les statistiques du flux"
            onToggled: if (Projection.bridge)
                Projection.bridge.setStatsHidden(checked)
        }

        BrumeToggle {
            objectName: "preferencesLoggingEnabled"
            Layout.fillWidth: true
            label: "Activer les journaux de diagnostic"
            checked: Projection.bridge
                && Projection.bridge.loggingEnabledPreference
            enabled: Projection.ready && !view.profile.closing
            Accessible.name: "Activer les journaux de diagnostic"
            onToggled: if (Projection.bridge)
                Projection.bridge.setLoggingEnabled(checked)
        }

        BrumeLabel {
            Layout.fillWidth: true
            variant: "note"
            text: "Active les fichiers journaux et les traces techniques du terminal. Les journaux existants sont conservés en cas de désactivation."
        }

        BrumeButton {
            objectName: "preferencesReset"
            Layout.fillWidth: true
            label: "Réinitialiser le fichier enregistré…"
            enabled: !view.profile.closing && view.settings.path.length > 0
            Accessible.name: "Réinitialiser le fichier de réglages avec confirmation"
            onClicked: if (Projection.bridge)
                Projection.bridge.requestSettingsReset()
        }

        BrumeLabel {
            Layout.fillWidth: true
            variant: "note"
            text: "La réinitialisation efface tous les profils de source et la dernière source enregistrée, en conservant les préférences globales actuelles. La capture en cours reste inchangée."
        }

        BrumeLabel {
            objectName: "preferencesRefused"
            Layout.fillWidth: true
            visible: view.settings.refused
            variant: "error"
            text: "Fichier de réglages refusé : les changements restent valables pour la session."
        }
    }
}
