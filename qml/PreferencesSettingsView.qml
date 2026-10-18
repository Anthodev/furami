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
            text: "Preferences"
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
            text: "Immediate: volume " + view.profile.live.volumePercent + " %"
                + " · mute " + (view.profile.live.muted ? "enabled" : "disabled")
                + " · window " + (Projection.bridge && Projection.bridge.fullscreenPreference
                    ? "full screen" : "windowed")
        }

        BrumeToggle {
            objectName: "preferencesStatsHidden"
            Layout.fillWidth: true
            label: "Hide stream statistics"
            checked: Projection.bridge
                && Projection.bridge.statsHiddenPreference
            enabled: Projection.ready && !view.profile.closing
            Accessible.name: "Hide stream statistics"
            onToggled: if (Projection.bridge)
                Projection.bridge.setStatsHidden(checked)
        }

        BrumeToggle {
            objectName: "preferencesLoggingEnabled"
            Layout.fillWidth: true
            label: "Enable diagnostic logging"
            checked: Projection.bridge
                && Projection.bridge.loggingEnabledPreference
            enabled: Projection.ready && !view.profile.closing
            Accessible.name: "Enable diagnostic logging"
            onToggled: if (Projection.bridge)
                Projection.bridge.setLoggingEnabled(checked)
        }

        BrumeLabel {
            Layout.fillWidth: true
            variant: "note"
            text: "Enables log files and terminal technical traces. Existing logs are kept when disabled."
        }

        BrumeButton {
            objectName: "preferencesReset"
            Layout.fillWidth: true
            label: "Reset saved file…"
            enabled: !view.profile.closing && view.settings.path.length > 0
            Accessible.name: "Reset the settings file with confirmation"
            onClicked: if (Projection.bridge)
                Projection.bridge.requestSettingsReset()
        }

        BrumeLabel {
            Layout.fillWidth: true
            variant: "note"
            text: "Reset clears every source profile and the last saved source, keeping the current global preferences. The running capture stays unchanged."
        }

        BrumeLabel {
            objectName: "preferencesRefused"
            Layout.fillWidth: true
            visible: view.settings.refused
            variant: "error"
            text: "Settings file refused: changes remain valid for this session."
        }
    }
}
