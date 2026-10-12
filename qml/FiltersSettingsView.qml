pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Filter settings.
//
// Existing loaded entries are shown losslessly and toggled in place: labels,
// order, repeated kinds and parameter data are never rewritten. Adding a
// missing kind appends the documented default chosen by the runtime; there is
// no generic chain editor.
Item {
    id: view

    implicitWidth: Brume.settingsWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property var filters: profile.filters
    readonly property var entries: profile.filters.entries

    function kindPresent(kind) {
        const list = view.entries
        for (let i = 0; i < list.length; ++i) {
            if (String(list[i].kind) === kind)
                return true
        }
        return false
    }

    function summarise(entry) {
        const p = entry.parameters || {}
        switch (String(entry.kind)) {
        case "format":
            return "matrice " + p.matrix + " · niveaux " + p.levels + " · gamma " + p.gamma
        case "eq":
            return "contraste " + p.contrast + " · luminosité " + p.brightness
                + " · saturation " + p.saturation + " · gamma " + p.gamma
                + " · gamma_r " + p.gamma_r + " · gamma_g " + p.gamma_g
                + " · gamma_b " + p.gamma_b + " · gamma_weight " + p.gamma_weight
        case "unsharp":
            return "luma " + p.luma_msize_x + "×" + p.luma_msize_y + " @ " + p.luma_amount
                + " · chroma " + p.chroma_msize_x + "×" + p.chroma_msize_y + " @ " + p.chroma_amount
                + " · alpha " + p.alpha_msize_x + "×" + p.alpha_msize_y + " @ " + p.alpha_amount
        case "hqdn3d":
            return "spatial " + p.luma_spatial + "/" + p.chroma_spatial
                + " · temporel " + p.luma_tmp + "/" + p.chroma_tmp
        case "bwdif":
            return "mode " + p.mode + " · parité " + p.parity + " · " + p.deint
        }
        return ""
    }

    function defaultNotice(kind) {
        switch (kind) {
        case "eq":
            return "Réglages neutres"
        case "unsharp":
            return "accentue la luminance"
        case "hqdn3d":
            return "débruitage 4/3/6/4.5"
        case "bwdif":
            return "désentrelace toujours"
        case "format":
            return "aucun retaggage"
        }
        return ""
    }

    function setEnabled(entryKey, enabled) {
        if (!Projection.bridge)
            return
        Projection.bridge.setFilterEnabled(profile.selectionRevision, entryKey, enabled === true)
    }
    function addKind(kind) {
        if (!Projection.bridge)
            return
        Projection.bridge.addFilter(profile.selectionRevision, kind)
    }
    // Remove exactly one loaded entry. The opaque revision-scoped key is the
    // identity; the runtime refuses a stale key or revision before touching
    // the chain, so removing the last entry and re-adding stays usable.
    function removeFilter(entryKey) {
        if (!Projection.bridge)
            return
        Projection.bridge.removeFilter(profile.selectionRevision, entryKey)
    }

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gap

        BrumeLabel {
            Layout.fillWidth: true
            text: "Filtres"
            variant: "title"
        }

        BrumeLabel {
            objectName: "filtersError"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "error"
            text: view.filters.error
        }

        BrumeLabel {
            Layout.fillWidth: true
            text: "Chaîne de filtres (" + view.entries.length + ")"
            variant: "secondary"
        }

        ColumnLayout {
            id: entryList
            Layout.fillWidth: true
            spacing: Brume.gapSmall
            enabled: view.profile.canEditSelection && view.profile.selection.present

                Repeater {
                    model: view.entries

                    ColumnLayout {
                        id: entryBlock
                        required property var modelData
                        Layout.fillWidth: true
                        spacing: 2

                        RowLayout {
                            Layout.fillWidth: true
                            spacing: Brume.gapSmall

                            BrumeToggle {
                                objectName: "filterToggle_" + entryBlock.modelData.key
                                Layout.fillWidth: true
                                label: String(entryBlock.modelData.label || "")
                                checked: entryBlock.modelData.enabled === true
                                enabled: entryBlock.modelData.available !== false
                                Accessible.name: String(entryBlock.modelData.label || "")
                                onClicked: view.setEnabled(String(entryBlock.modelData.key), checked)
                            }
                            // Separate sibling control: pressing remove never
                            // reaches the toggle. Enabled for both checked and
                            // unchecked rows, always with an explicit label.
                            BrumeButton {
                                objectName: "filterRemove_" + entryBlock.modelData.key
                                Layout.alignment: Qt.AlignVCenter
                                label: "Retirer"
                                Accessible.name: "Retirer "
                                    + String(entryBlock.modelData.label || "")
                                onClicked: view.removeFilter(String(entryBlock.modelData.key))
                            }
                        }
                        BrumeLabel {
                            Layout.fillWidth: true
                            leftPadding: 28
                            variant: "note"
                            text: view.summarise(entryBlock.modelData)
                                + (String(entryBlock.modelData.reason || "").length > 0
                                    ? " — " + String(entryBlock.modelData.reason) : "")
                        }
                    }
            }
        }

        BrumeLabel {
            Layout.fillWidth: true
            visible: view.entries.length === 0
            variant: "note"
            text: "Chaîne vide."
        }

        Rectangle {
            Layout.fillWidth: true
            implicitHeight: Brume.dividerWidth
            color: Brume.divider
        }

        BrumeLabel {
            Layout.fillWidth: true
            text: "Ajouter un filtre disponible"
            variant: "secondary"
        }

        Repeater {
            model: view.filters.kinds

            BrumeButton {
                id: kindButton
                required property var modelData
                objectName: "filterAdd_" + kindButton.modelData.kind
                Layout.fillWidth: true
                label: "Ajouter " + String(kindButton.modelData.label || kindButton.modelData.kind)
                    + (view.defaultNotice(String(kindButton.modelData.kind)).length > 0
                        ? " (" + view.defaultNotice(String(kindButton.modelData.kind)) + ")" : "")
                enabled: kindButton.modelData.available === true
                    && !view.kindPresent(String(kindButton.modelData.kind))
                    && view.profile.canEditSelection
                    && view.profile.selection.present
                Accessible.name: label
                onClicked: view.addKind(String(kindButton.modelData.kind))
            }
        }

        BrumeLabel {
            Layout.fillWidth: true
            visible: view.filters.kinds.length === 0
            variant: "note"
            text: view.filters.available
                ? "Aucun type de filtre disponible."
                : "Capacités de filtrage indisponibles."
        }
    }
}
