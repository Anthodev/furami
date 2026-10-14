pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import dev.antho.furami

// Themed selector over an array of opaque-keyed rows.
//
// Rows are runtime-projected objects; labels are external data and are only
// ever rendered as plain text. Selection reports the opaque row key, never an
// index, a device name or a FourCC.
ComboBox {
    id: control

    // Array of { key, label, eligible, reason, ... } objects.
    property var rows: []
    // Opaque key of the currently projected choice. "" means "nothing selected".
    property string selectedKey: ""
    // Shown when the projected choice has no matching row.
    property string emptyText: "—"
    property bool refreshCatalogOnOpen: false

    focusPolicy: Qt.StrongFocus
    Keys.onPressed: function(event) { Chrome.noteKeyboardActivity() }
    font.family: Brume.fontFamily
    font.pixelSize: Brume.fontSize
    implicitHeight: Brume.controlHeight
    leftPadding: Brume.gap
    rightPadding: Brume.gap + 24
    spacing: Brume.gapSmall
    palette.window: Brume.menuSurface
    palette.windowText: enabled ? Brume.foreground : Brume.disabledText
    palette.buttonText: enabled ? Brume.foreground : Brume.disabledText
    palette.text: enabled ? Brume.foreground : Brume.disabledText
    palette.base: Brume.control
    palette.highlight: Brume.controlHover
    palette.highlightedText: Brume.foreground
    palette.mid: Brume.divider
    palette.dark: Brume.divider

    model: control.rows
    textRole: "label"

    onPressedChanged: Chrome.setInteractionHeld(control.pressed)

    // The runtime projects the current choice as an opaque key. A key that is
    // not present stays visibly selected-but-unavailable (index -1) and is
    // never mapped onto another row.
    currentIndex: {
        const key = control.selectedKey
        const list = control.rows
        if (key.length === 0)
            return -1
        for (let i = 0; i < list.length; ++i) {
            if (String(list[i].key) === key)
                return i
        }
        return -1
    }

    // The projected key is authoritative: the displayed label never depends on
    // a currentIndex that user interaction can move away from the projection.
    displayText: {
        const list = control.rows
        const key = control.selectedKey
        if (key.length > 0) {
            for (let i = 0; i < list.length; ++i) {
                if (String(list[i].key) === key)
                    return String(list[i].label)
            }
        }
        return control.emptyText
    }

    delegate: ItemDelegate {
        id: rowDelegate
        required property var modelData
        required property int index
        objectName: control.objectName + "_row_" + rowDelegate.modelData.key
        width: control.width
        implicitHeight: Brume.controlHeight
        enabled: rowDelegate.modelData.eligible !== false
        highlighted: control.highlightedIndex === index

        contentItem: BrumeLabel {
            text: String(rowDelegate.modelData.label)
            variant: rowDelegate.modelData.eligible === false ? "secondary" : "body"
            elide: Text.ElideRight
            verticalAlignment: Text.AlignVCenter
            Accessible.name: rowDelegate.modelData.eligible === false
                ? text + ", " + String(rowDelegate.modelData.reason || "unavailable")
                : text
        }

        background: Rectangle {
            color: rowDelegate.highlighted ? Brume.controlHover
                : rowDelegate.hovered ? Brume.control : "transparent"
            radius: Brume.radiusControl
        }
    }

    contentItem: BrumeLabel {
        id: contentLabel
        text: control.displayText
        elide: Text.ElideRight
        verticalAlignment: Text.AlignVCenter
    }

    indicator: BrumeIcon {
        x: control.width - control.rightPadding + Brume.gapSmall
        y: control.topPadding + (control.availableHeight - height) / 2
        iconSize: 16
        source: Brume.icon("chevron-down")
        tint: control.enabled ? Brume.secondary : Brume.disabledText
        rotation: control.popup.visible ? 180 : 0
        Behavior on rotation {
            NumberAnimation { duration: Brume.hoverDuration }
        }
    }

    background: Rectangle {
        radius: Brume.radiusControl
        color: control.enabled ? Brume.control : Brume.disabledFill
        border.width: control.visualFocus ? 2 : 1
        border.color: control.visualFocus ? Brume.accent : Brume.divider
    }

    popup: Popup {
        popupType: Popup.Item
        onOpened: if (control.refreshCatalogOnOpen) Chrome.refreshCatalogs()
        y: control.height + Brume.gapSmall
        width: control.width
        implicitHeight: Math.min(contentItem.implicitHeight + 2, 320)
        padding: 1
        background: Rectangle {
            radius: Brume.radiusSurface
            color: Brume.menuSurface
            border.width: 1
            border.color: Brume.divider
        }
        contentItem: ListView {
            clip: true
            implicitHeight: contentHeight
            model: control.popup.visible ? control.delegateModel : null
            currentIndex: control.highlightedIndex
            ScrollIndicator.vertical: ScrollIndicator {}
        }
    }
}
