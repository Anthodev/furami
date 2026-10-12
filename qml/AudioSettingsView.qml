import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Audio settings: captured audio (immediate selection) kept distinct from the
// playback output route and mute (immediate preferences).
Item {
    id: view

    implicitWidth: Brume.settingsWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property var selection: profile.selection
    readonly property var catalog: profile.catalog
    readonly property bool captureEditable: profile.canEditSelection && selection.present

    function chooseInput(rowKey) {
        if (!Projection.bridge)
            return
        Projection.bridge.selectAudioInput(profile.selectionRevision, catalog.revision, rowKey)
    }
    function chooseOutput(rowKey) {
        if (!Projection.bridge)
            return
        Projection.bridge.selectOutput(rowKey, profile.output.revision)
    }
    function refresh() {
        if (Projection.bridge)
            Projection.bridge.refreshCatalogs()
    }

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gap

        BrumeLabel {
            Layout.fillWidth: true
            text: "Audio"
            variant: "title"
        }

        // ── Captured audio (immediate selection) ──────────────────────────
        BrumeLabel {
            Layout.fillWidth: true
            text: "Audio capturé"
            variant: "secondary"
        }

        BrumeToggle {
            objectName: "audioCaptureEnabled"
            Layout.fillWidth: true
            label: "Capturer l'audio"
            checked: view.selection.audioEnabled
            enabled: view.captureEditable
            Accessible.name: "Capturer l'audio depuis la source"
            onClicked: if (Projection.bridge)
                Projection.bridge.setAudioEnabled(view.profile.selectionRevision, checked)
        }

        BrumeLabel {
            objectName: "audioMissingInput"
            Layout.fillWidth: true
            visible: !view.selection.audioEnabled && view.selection.audioInputKey.length === 0
            variant: "note"
            text: "Aucune entrée sélectionnée : choisissez une entrée puis activez la capture audio."
        }

        RowLayout {
            Layout.fillWidth: true
            spacing: Brume.gapSmall
            BrumeChoice {
                objectName: "audioInputSelector"
                refreshCatalogOnOpen: true
                Layout.fillWidth: true
                enabled: view.captureEditable
                rows: view.catalog.audioInputRows
                selectedKey: view.selection.audioInputKey
                emptyText: "Aucune entrée"
                Accessible.name: "Entrée audio capturée"
                onActivated: {
                    if (currentIndex < 0)
                        return
                    view.chooseInput(String(rows[currentIndex].key))
                }
            }
            BrumeIconButton {
                objectName: "audioRefresh"
                label: "Actualiser les catalogues"
                icon.source: Brume.icon("refresh")
                enabled: !view.catalog.busy && !view.profile.closing
                onClicked: view.refresh()
            }
        }

        BrumeLabel {
            objectName: "audioRetained"
            Layout.fillWidth: true
            visible: view.selection.audioInputLabel.length > 0
            variant: "note"
            text: "Entrée conservée quand l'audio est désactivé : " + view.selection.audioInputLabel
        }

        BrumeLabel {
            objectName: "audioCaptureStatus"
            Layout.fillWidth: true
            variant: "note"
            text: "État : " + (view.profile.live.audioStatus.length > 0
                    ? view.profile.live.audioStatus : "—")
                + (view.profile.live.audioDiagnostic.length > 0
                    ? "\n" + view.profile.live.audioDiagnostic : "")
        }

        Rectangle {
            Layout.fillWidth: true
            implicitHeight: Brume.dividerWidth
            color: Brume.divider
        }

        // ── Playback output route (immediate) ─────────────────────────────
        BrumeLabel {
            Layout.fillWidth: true
            text: "Sortie de lecture"
            variant: "secondary"
        }

        BrumeChoice {
            objectName: "audioOutputSelector"
            Layout.fillWidth: true
            rows: view.profile.output.rows
            selectedKey: view.profile.output.selectedKey
            emptyText: view.profile.output.selectedLabel
            Accessible.name: "Sortie audio de lecture"
            onActivated: {
                if (currentIndex < 0)
                    return
                view.chooseOutput(String(rows[currentIndex].key))
            }
        }

        BrumeToggle {
            objectName: "audioOutputMute"
            Layout.fillWidth: true
            label: view.profile.live.muted ? "Sourdine active" : "Sourdine"
            checked: view.profile.live.muted
            Accessible.name: "Sourdine de lecture"
            onClicked: if (Projection.bridge)
                Projection.bridge.setMuted(checked)
        }

        BrumeLabel {
            objectName: "audioOutputStatus"
            Layout.fillWidth: true
            visible: view.profile.output.status.length > 0
                || view.profile.output.effectiveLabel.length > 0
            variant: "note"
            text: view.profile.output.status
                + (view.profile.output.effectiveLabel.length > 0
                    ? "\nSortie active : " + view.profile.output.effectiveLabel : "")
        }

        BrumeLabel {
            Layout.fillWidth: true
            variant: "note"
            text: "La sourdine ne remplace pas la route sélectionnée ; la sortie reste choisie."
        }
    }
}
