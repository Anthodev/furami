import QtQuick
import QtQuick.Controls
import dev.antho.furami

// Themed Popup.Item used for every in-tool surface (selectors, menu, dialogs).
// It never creates a window: the three chrome tools stay the only native
// surfaces, so bounds stay bounded to their owner tool.
Popup {
    id: control
    property bool mandatory: false
    property real maximumWidth: Brume.popupMaxWidth + 2 * Brume.edge
    property real maximumHeight: 480
    width: Math.min(implicitWidth, maximumWidth)
    height: Math.min(implicitHeight, maximumHeight)

    popupType: Popup.Item
    modal: true
    dim: false
    focus: true
    padding: Brume.edge
    closePolicy: mandatory ? Popup.NoAutoClose
        : Popup.CloseOnEscape | Popup.CloseOnPressOutside

    // Qt 6.11 Popup.Item already provides a native Tab/Backtab fence. Only
    // initial/boundary focus is explicit; ordinary traversal stays with Qt.
    function boundaryItem(node, backward) {
        const children = node.children
        for (let i = 0; i < children.length; ++i) {
            const child = children[backward ? children.length - 1 - i : i]
            if (!child.visible || !child.enabled)
                continue
            const nested = control.boundaryItem(child, backward)
            if (nested)
                return nested
            if (child.activeFocusOnTab)
                return child
        }
        return null
    }
    function focusBoundary(backward) {
        const target = control.boundaryItem(control.contentItem, backward) || control.contentItem
        target.forceActiveFocus(Qt.PopupFocusReason)
    }

    background: Rectangle {
        radius: Brume.radiusSurface
        color: Brume.menuSurface
        border.width: 1
        border.color: Brume.divider
    }

    enter: Transition {
        NumberAnimation {
            property: "opacity"
            from: 0
            to: 1
            duration: Brume.hoverDuration
            easing: Brume.fadeEasing
        }
    }
    exit: Transition {
        NumberAnimation {
            property: "opacity"
            from: 1
            to: 0
            duration: Brume.hoverDuration
            easing: Brume.fadeEasing
        }
    }
}
