import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Preferences: the immediate playback preferences (stream statistics, diagnostic
// logging) and the real saved-file reset intent (the confirmation itself is an
// existing token-checked dialog). The persisted technical facts live behind a
// local, never-persisted "Technical details" disclosure; persistence errors stay
// outside it so they are visible whatever the disclosure state.
Item {
    id: view

    implicitWidth: Brume.settingsWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property var settings: profile.settings

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gapLarge

        // Projected attention text: refusals, pending legacy migration, write
        // durability and startup restoration problems. Always visible, never
        // inside the disclosure.
        BrumeLabel {
            objectName: "preferencesAttention"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "error"
            text: view.settings.attention
        }

        // ── Display ───────────────────────────────────────────────────────
        ColumnLayout {
            Layout.fillWidth: true
            spacing: Brume.gapSmall

            BrumeLabel {
                Layout.fillWidth: true
                text: "Display"
                font.weight: Brume.fontWeightStrong
            }

            BrumeToggle {
                objectName: "preferencesStatsHidden"
                Layout.fillWidth: true
                label: "Show stream statistics"
                checked: Projection.bridge
                    && !Projection.bridge.statsHiddenPreference
                enabled: Projection.ready && !view.profile.closing
                Accessible.name: "Show stream statistics"
                onToggled: if (Projection.bridge)
                    Projection.bridge.setStatsHidden(!checked)
            }
        }

        // ── Diagnostics ───────────────────────────────────────────────────
        ColumnLayout {
            Layout.fillWidth: true
            spacing: Brume.gapSmall

            BrumeLabel {
                Layout.fillWidth: true
                text: "Diagnostics"
                font.weight: Brume.fontWeightStrong
            }

            BrumeToggle {
                objectName: "preferencesLoggingEnabled"
                Layout.fillWidth: true
                label: "Diagnostic logging"
                checked: Projection.bridge
                    && Projection.bridge.loggingEnabledPreference
                enabled: Projection.ready && !view.profile.closing
                Accessible.name: "Diagnostic logging"
                onToggled: if (Projection.bridge)
                    Projection.bridge.setLoggingEnabled(checked)
            }

            BrumeLabel {
                Layout.fillWidth: true
                variant: "note"
                text: "Write diagnostic logs and technical terminal output. Existing logs are kept when disabled."
            }
        }

        Rectangle {
            Layout.fillWidth: true
            implicitHeight: Brume.dividerWidth
            color: Brume.divider
        }

        // ── Saved settings ────────────────────────────────────────────────
        ColumnLayout {
            Layout.fillWidth: true
            spacing: Brume.gapSmall

            BrumeLabel {
                Layout.fillWidth: true
                text: "Saved settings"
                font.weight: Brume.fontWeightStrong
            }

            BrumeLabel {
                Layout.fillWidth: true
                variant: "note"
                text: "Clears saved source profiles and the last source. Keeps global preferences and the current capture."
            }

            BrumeButton {
                objectName: "preferencesReset"
                Layout.alignment: Qt.AlignLeft
                label: "Reset saved file…"
                enabled: !view.profile.closing && view.settings.path.length > 0
                Accessible.name: "Reset the settings file with confirmation"
                onClicked: if (Projection.bridge)
                    Projection.bridge.requestSettingsReset()
            }
        }

        // ── Technical details (local disclosure) ──────────────────────────
        BrumeButton {
            id: technicalDetails
            objectName: "preferencesTechnicalDetails"
            Layout.fillWidth: true
            label: "Technical details"
            checkable: true
            checked: false
            // The accessible name stays the inherited BrumeButton label
            // ("Technical details"); the disclosure hint is a description.
            Accessible.description: technicalDetails.checked
                ? "Collapse technical details" : "Expand technical details"
            contentItem: RowLayout {
                spacing: technicalDetails.spacing
                BrumeLabel {
                    Layout.fillWidth: true
                    Layout.minimumWidth: 0
                    text: technicalDetails.label
                    font: technicalDetails.font
                    color: technicalDetails.enabled ? Brume.foreground : Brume.disabledText
                    horizontalAlignment: Text.AlignLeft
                    verticalAlignment: Text.AlignVCenter
                    wrapMode: Text.NoWrap
                    elide: Text.ElideRight
                }
                BrumeIcon {
                    source: Brume.icon("chevron-down")
                    iconSize: technicalDetails.icon.width
                    tint: technicalDetails.icon.color
                    rotation: technicalDetails.checked ? 180 : 0
                    Layout.alignment: Qt.AlignVCenter
                }
            }
        }

        ColumnLayout {
            Layout.fillWidth: true
            visible: technicalDetails.checked
            spacing: Brume.gapSmall

            ColumnLayout {
                Layout.fillWidth: true
                spacing: 0

                BrumeLabel {
                    Layout.fillWidth: true
                    text: "Saved settings"
                    variant: "note"
                    font.weight: Brume.fontWeightStrong
                }

                BrumeLabel {
                    objectName: "preferencesStatus"
                    Layout.fillWidth: true
                    variant: "note"
                    text: view.settings.status
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                visible: view.settings.startupReason.length > 0
                spacing: 0

                BrumeLabel {
                    Layout.fillWidth: true
                    text: "Startup restoration"
                    variant: "note"
                    font.weight: Brume.fontWeightStrong
                }

                BrumeLabel {
                    objectName: "preferencesStartup"
                    Layout.fillWidth: true
                    visible: text.length > 0
                    variant: "note"
                    text: view.settings.startupReason
                }
            }

            ColumnLayout {
                Layout.fillWidth: true
                visible: view.settings.path.length > 0
                spacing: 0

                BrumeLabel {
                    Layout.fillWidth: true
                    text: "Settings file"
                    variant: "note"
                    font.weight: Brume.fontWeightStrong
                }

                BrumeLabel {
                    objectName: "preferencesPath"
                    Layout.fillWidth: true
                    visible: view.settings.path.length > 0
                    variant: "note"
                    text: view.settings.path
                }
            }

            BrumeLabel {
                objectName: "preferencesImmediate"
                Layout.fillWidth: true
                variant: "secondary"
                text: "Current session: volume " + view.profile.live.volumePercent + " %"
                    + " · mute " + (view.profile.live.muted ? "enabled" : "disabled")
                    + " · window " + (Projection.bridge && Projection.bridge.fullscreenPreference
                        ? "full screen" : "windowed")
            }
        }
    }
}
