pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Bottom-right menu: the concise root (playback, restart, settings, close) and
// the same-frame settings sections. Every complete selection is applied
// immediately by its own section; there is no visible pending draft.
Item {
    id: menu

    // root | settings | capture | audio | filters | renderer | prefs
    property string section: "root"
    property bool editorActive: false

    readonly property var profile: Projection.view
    readonly property bool settingsSection: section !== "root" && section !== "settings"
    // The Options menu runs 20% wider than the shared settings baseline. This
    // seam is menu-only: the standalone settings views and the source/output
    // selector popups keep their own width tokens. The owner popup still clamps
    // to the tool width on a small viewport.
    readonly property int baseWidth: menu.settingsSection ? Brume.settingsWidth : Brume.menuWidth
    implicitWidth: Math.round(menu.baseWidth * 1.2)
    implicitHeight: layout.implicitHeight
    onSectionChanged: {
        if (menu.section === "capture" || menu.section === "audio")
            Chrome.refreshCatalogs()
    }
    Timer {
        interval: 2000
        repeat: true
        running: menu.visible && Chrome.rootAvailable && Chrome.popup === "menu"
            && (menu.section === "capture" || menu.section === "audio")
        onTriggered: Chrome.refreshCatalogs()
    }

    function goBack() {
        switch (menu.section) {
        case "root":
            Chrome.closePopup()
            break
        case "settings":
            menu.section = "root"
            break
        default:
            menu.section = "settings"
            break
        }
    }
    function chooseSettings(target) {
        menu.section = target
    }

    function filterSummary() {
        const list = menu.profile.filters.entries
        let enabled = 0
        for (let i = 0; i < list.length; ++i) {
            if (list[i].enabled === true)
                ++enabled
        }
        return list.length + " filtre(s) · " + enabled + " actif(s)"
    }

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gap

        // ── Head ──────────────────────────────────────────────────────────
        ColumnLayout {
            Layout.fillWidth: true
            spacing: 0
            BrumeLabel {
                objectName: "menuTitle"
                Layout.fillWidth: true
                text: menu.section === "root" ? "Capture"
                    : menu.section === "settings" ? "Réglages"
                    : menu.section === "capture" ? "Capture — format"
                    : menu.section === "audio" ? "Audio"
                    : menu.section === "filters" ? "Filtres"
                    : menu.section === "renderer" ? "Rendu"
                    : "Préférences"
                variant: "title"
            }
            BrumeLabel {
                objectName: "menuSubtitle"
                Layout.fillWidth: true
                variant: "note"
                text: Projection.productPhaseText(menu.profile.productPhase)
                    + (menu.profile.live.sourceLabel.length > 0
                        ? " · " + menu.profile.live.sourceLabel : "")
                    + (menu.profile.live.sizeLabel.length > 0
                        ? " · " + menu.profile.live.sizeLabel : "")
                    + (menu.profile.live.rateLabel.length > 0
                        ? " · " + Projection.rateFps(menu.profile.live.rateLabel) : "")
            }
        }

        // ── Root ──────────────────────────────────────────────────────────
        ColumnLayout {
            Layout.fillWidth: true
            visible: menu.section === "root"
            spacing: Brume.gapSmall

            BrumeButton {
                objectName: "menuPauseResume"
                Layout.fillWidth: true
                icon.source: Brume.icon(menu.profile.paused ? "play" : "pause")
                label: menu.profile.paused ? "Reprendre" : "Mettre en pause"
                enabled: menu.profile.canTogglePause
                Accessible.name: label
                onClicked: if (Projection.bridge)
                    Projection.bridge.togglePause()
            }
            BrumeButton {
                objectName: "menuRestart"
                Layout.fillWidth: true
                icon.source: Brume.icon("refresh")
                label: "Redémarrer la capture"
                enabled: menu.profile.canRestart
                Accessible.name: label
                onClicked: if (Projection.bridge)
                    Projection.bridge.restartCapture(menu.profile.generation)
            }
            BrumeButton {
                objectName: "menuReconnect"
                Layout.fillWidth: true
                visible: menu.profile.canReconnect
                icon.source: Brume.icon("refresh")
                label: "Reconnecter la capture"
                enabled: menu.profile.canReconnect && !menu.profile.closing
                Accessible.name: label
                onClicked: if (Projection.bridge)
                    Projection.bridge.reconnectCapture(menu.profile.generation)
            }
            // A complete selection that is not running yet (explicit startup
            // selection or a closed capture) stays openable without any Apply.
            // The reconnect affordance already covers the disconnected case.
            BrumeButton {
                objectName: "menuOpenCapture"
                Layout.fillWidth: true
                readonly property bool openable: Projection.bridge
                    ? Projection.bridge.canOpen === true : false
                visible: openable && !menu.profile.canReconnect
                icon.source: Brume.icon("play")
                label: "Ouvrir la capture"
                Accessible.name: label
                onClicked: if (Projection.bridge)
                    Projection.bridge.openCapture()
            }
            BrumeButton {
                objectName: "menuSettings"
                Layout.fillWidth: true
                icon.source: Brume.icon("settings")
                label: "Réglages"
                Accessible.name: label
                onClicked: menu.chooseSettings("settings")
            }
            BrumeButton {
                objectName: "menuCloseCapture"
                Layout.fillWidth: true
                icon.source: Brume.icon("x")
                label: "Fermer la capture"
                enabled: !menu.profile.closing
                Accessible.name: label
                onClicked: if (Projection.bridge)
                    Projection.bridge.closeCapture()
            }
        }

        // ── Settings index ────────────────────────────────────────────────
        ColumnLayout {
            Layout.fillWidth: true
            visible: menu.section === "settings"
            spacing: Brume.gapSmall

            BrumeButton {
                objectName: "menuBack"
                Layout.fillWidth: true
                icon.source: Brume.icon("chevron-down")
                label: "Retour"
                Accessible.name: "Retour au menu"
                onClicked: menu.goBack()
            }
            BrumeButton {
                objectName: "settingsCaptureEntry"
                Layout.fillWidth: true
                implicitHeight: 52
                icon.source: Brume.icon("camera")
                label: "Capture\n" + (menu.profile.selection.sourceLabel.length > 0
                    ? menu.profile.selection.sourceLabel : "aucune source")
                Accessible.name: "Réglages de capture"
                onClicked: menu.chooseSettings("capture")
            }
            BrumeButton {
                objectName: "settingsAudioEntry"
                Layout.fillWidth: true
                implicitHeight: 52
                icon.source: Brume.icon("headphones")
                label: "Audio\n" + (menu.profile.selection.audioEnabled
                    ? (menu.profile.selection.audioInputLabel.length > 0
                        ? menu.profile.selection.audioInputLabel : "entrée à choisir")
                    : "désactivé")
                Accessible.name: "Réglages audio"
                onClicked: menu.chooseSettings("audio")
            }
            BrumeButton {
                objectName: "settingsFiltersEntry"
                Layout.fillWidth: true
                implicitHeight: 52
                icon.source: Brume.icon("filter")
                label: "Filtres\n" + menu.filterSummary()
                Accessible.name: "Réglages des filtres"
                onClicked: menu.chooseSettings("filters")
            }
            BrumeButton {
                objectName: "settingsRendererEntry"
                Layout.fillWidth: true
                implicitHeight: 52
                icon.source: Brume.icon("palette")
                label: "Rendu\n" + (menu.profile.selection.renderer.scale !== undefined
                    ? String(menu.profile.selection.renderer.scale)
                        + " / " + String(menu.profile.selection.renderer.dscale) : "—")
                Accessible.name: "Réglages de rendu"
                onClicked: menu.chooseSettings("renderer")
            }
            BrumeButton {
                objectName: "settingsPreferencesEntry"
                Layout.fillWidth: true
                implicitHeight: 52
                icon.source: Brume.icon("adjustments")
                label: "Préférences\n" + (menu.profile.settings.status.length > 0
                    ? menu.profile.settings.status : "—")
                Accessible.name: "Préférences"
                onClicked: menu.chooseSettings("prefs")
            }
        }

        // ── Sections ──────────────────────────────────────────────────────
        ColumnLayout {
            Layout.fillWidth: true
            visible: menu.settingsSection
            spacing: Brume.gapSmall

            BrumeButton {
                id: sectionBack
                objectName: "menuBackSection"
                Layout.fillWidth: true
                icon.source: Brume.icon("chevron-down")
                label: "Retour"
                Accessible.name: "Retour aux réglages"
                onClicked: menu.goBack()
            }

            BrumeScrollView {
                Layout.fillWidth: true
                Layout.preferredHeight: Math.min(sectionLoader.implicitHeight, Brume.popupMaxListHeight)
                clip: true
                contentWidth: availableWidth
                contentHeight: sectionLoader.implicitHeight
                ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
                ScrollBar.vertical.policy: ScrollBar.AsNeeded
                focus: true

                Loader {
                    id: sectionLoader
                    width: Math.max(0, parent.width)
                    sourceComponent: {
                        switch (menu.section) {
                        case "capture":
                            return captureSection
                        case "audio":
                            return audioSection
                        case "filters":
                            return filtersSection
                        case "renderer":
                            return rendererSection
                        case "prefs":
                            return preferencesSection
                        }
                        return null
                    }
                }
            }
        }

        // ── Typed intent error ────────────────────────────────────────────
        BrumeLabel {
            objectName: "menuIntentError"
            Layout.fillWidth: true
            visible: menu.profile.intentError !== null
            variant: "error"
            text: menu.profile.intentError
                ? (menu.profile.intentError.message.length > 0
                    ? menu.profile.intentError.message : menu.profile.intentError.code)
                : ""
        }
    }

    Component {
        id: captureSection
        CaptureSettingsView {}
    }
    Component {
        id: audioSection
        AudioSettingsView {}
    }
    Component {
        id: filtersSection
        FiltersSettingsView {
            editorActive: menu.editorActive && menu.visible && menu.section === "filters"
                && !sectionBack.pressed && Chrome.rootAvailable && Chrome.popup === "menu"
        }
    }
    Component {
        id: rendererSection
        RendererSettingsView {}
    }
    Component {
        id: preferencesSection
        PreferencesSettingsView {}
    }
}
