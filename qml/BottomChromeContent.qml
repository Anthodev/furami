pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Bottom chrome content: the truthful session context and stream statistics on
// the left, the concise menu trigger on the right. The menu, settings sections
// and the existing close/reset/save confirmations all live in one bounded
// Popup.Item, so the tool only expands to a bounded surface and never to the
// full viewport.
Item {
    id: bottom

    objectName: "bottomChromeRoot"

    // Driven by the root composition: content fades, then the tool hides.
    property bool chromeShown: true
    property real availableHeight: 524
    opacity: chromeShown ? 1 : 0
    Behavior on opacity {
        NumberAnimation {
            duration: Brume.fadeDuration
            easing: Brume.fadeEasing
        }
    }

    readonly property var profile: Projection.view
    readonly property bool popupActive: Chrome.popupRegion(Chrome.popup) === "bottom"
    readonly property bool contextVisible: !Chrome.popupOpen

    // Structured stream statistics, hidden only by the persisted preference.
    // This never gates the operational `ContextStatus` surface.
    readonly property bool statsVisible: bottom.contextVisible
        && bottom.profile.streamStats !== null
        && Projection.bridge !== null
        && !Projection.bridge.statsHiddenPreference
    readonly property string statsText: {
        const s = bottom.profile.streamStats
        if (s === null)
            return ""
        const size = s.size
        const rate = s.rate
        return s.identityText
            + "\nInput policy: " + s.inputPolicy
            + " · " + size.width + "×" + size.height
            + " · " + Projection.rateFps(rate.text)
            + "\nResolved input: " + s.resolvedInput
            + "\nAudio " + (s.audioEnabled ? "enabled" : "disabled (retained)")
            + ": " + (s.audioInputLabel.length > 0 ? s.audioInputLabel : "none")
    }

    readonly property real chromeHeight: Math.min(bottom.availableHeight,
        bottom.popupActive ? bottomPopup.height + (stack.visible ? Brume.gap + stack.implicitHeight : 0)
                           : stack.implicitHeight)
    property int surfaceEpoch: -1
    property int closingEpoch: -1
    // Chrome.popup remains selected during the exit transition. Editing ends
    // at aboutToHide, before deferred field completions can run.
    property bool menuEditorActive: false

    signal extentChanged()
    onChromeHeightChanged: bottom.extentChanged()

    function reconcilePopup() {
        if (bottom.popupActive) {
            bottom.surfaceEpoch = Chrome.popupEpoch
            if (!bottomPopup.opened) {
                const epoch = bottom.surfaceEpoch
                Qt.callLater(function() {
                    if (bottom.popupActive && Chrome.rootAvailable && epoch === Chrome.popupEpoch)
                        bottomPopup.open()
                })
            }
        } else if (bottomPopup.visible) {
            bottomPopup.close()
        }
    }

    // The runtime owns the close/reset tokens: the matching confirmation opens
    // on its own and closes again when the token is consumed or cleared.
    function syncSessionDialogs() {
        Chrome.syncSessionDialogs()
    }

    function focusBoundary(backward) {
        if (bottom.popupActive) {
            // Mandatory restoration may follow a pointer press; only a real
            // Tab/Backtab event is allowed to enable keyboard navigation mode.
            if (bottomView.item && typeof bottomView.item.focusBoundary === "function")
                bottomView.item.focusBoundary(backward)
            else
                bottomPopup.focusBoundary(backward)
            return
        }
        Chrome.noteKeyboardActivity()
        const target = bottomPopup.boundaryItem(stack, backward)
        if (target)
            target.forceActiveFocus(Qt.TabFocusReason)
    }

    Keys.onPressed: function(event) {
        if (bottom.popupActive || (event.key !== Qt.Key_Tab && event.key !== Qt.Key_Backtab))
            return
        const backward = event.key === Qt.Key_Backtab
        const boundary = bottomPopup.boundaryItem(stack, !backward)
        if (boundary && bottom.Window.window.activeFocusItem === boundary) {
            Chrome.noteKeyboardActivity()
            if (Projection.bridge)
                Projection.bridge.focusNextChrome("bottom", backward)
            event.accepted = true
        }
    }

    Connections {
        target: Chrome
        function onPopupChanged() {
            bottom.reconcilePopup()
        }
    }
    Connections {
        target: Projection
        function onViewChanged() {
            bottom.syncSessionDialogs()
        }
    }
    Component.onCompleted: {
        bottom.reconcilePopup()
        bottom.syncSessionDialogs()
    }

    PointerActivity {}

    // Non-control parts of this native tool represent the video underneath.
    MouseArea {
        objectName: "bottomBackground"
        anchors.fill: parent
        z: -1
        acceptedButtons: Qt.LeftButton
        enabled: !Chrome.popupOpen && !bottomPopup.visible
        onPressed: {
            if (Projection.bridge)
                Projection.bridge.focusVideo()
        }
    }

    // Full available width: the operational context and the stream statistics
    // stay anchored bottom-left, the Options trigger bottom-right. The same
    // `stack` id keeps the keyboard boundary traversal and popup height math.
    RowLayout {
        id: stack
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        width: parent.width
        spacing: Brume.gap
        visible: !Chrome.mandatoryPopupOpen

        ColumnLayout {
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignBottom
            spacing: Brume.gapSmall

            ContextStatus {
                Layout.fillWidth: true
                visible: bottom.contextVisible
            }

            BrumeLabel {
                objectName: "streamStats"
                Layout.fillWidth: true
                visible: bottom.statsVisible
                variant: "note"
                text: bottom.statsText
            }
        }

        BrumeButton {
            id: optionsTrigger
            objectName: "optionsTrigger"
            Layout.alignment: Qt.AlignRight | Qt.AlignBottom
            icon.source: Brume.icon("adjustments")
            label: "Options"
            Accessible.name: "Capture options"
            enabled: Projection.ready && !Chrome.mandatoryPopupOpen
            onClicked: Chrome.openPopup("menu")
        }
    }

    BrumePopup {
        id: bottomPopup
        objectName: "bottomSurfacePopup"
        // Right-anchored to the tool, which is itself full available width.
        x: bottom.width - width
        y: 0
        mandatory: Chrome.mandatoryPopupOpen
        maximumWidth: bottom.width
        maximumHeight: Math.max(1, bottom.availableHeight
            - (stack.visible ? stack.implicitHeight + Brume.gap : 0))
        onAboutToHide: {
            bottom.menuEditorActive = false
            bottom.closingEpoch = bottom.surfaceEpoch
        }
        onOpened: {
            if (!bottom.popupActive || !Chrome.rootAvailable || bottom.surfaceEpoch !== Chrome.popupEpoch)
                return
            bottom.menuEditorActive = true
            bottom.Window.window.requestActivate()
            bottom.focusBoundary(false)
        }
        onClosed: {
            bottom.menuEditorActive = false
            const restoreInvoker = Chrome.popupClosed("bottom", bottom.closingEpoch)
            if (bottom.popupActive) {
                bottom.reconcilePopup()
                return
            }
            if (restoreInvoker && optionsTrigger.visible && optionsTrigger.enabled)
                optionsTrigger.forceActiveFocus(Qt.PopupFocusReason)
        }

        contentItem: BrumeScrollView {
            implicitWidth: bottomView.implicitWidth
            implicitHeight: bottomView.implicitHeight
            contentWidth: availableWidth
            contentHeight: bottomView.implicitHeight
            clip: true
            ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
            ScrollBar.vertical.policy: ScrollBar.AsNeeded
            Loader {
                id: bottomView
                width: parent.width
                enabled: Projection.ready
                onLoaded: {
                    const epoch = Chrome.popupEpoch
                    Qt.callLater(function() {
                        if (bottom.popupActive && bottomPopup.opened && Chrome.rootAvailable
                                && epoch === Chrome.popupEpoch) {
                            bottom.Window.window.requestActivate()
                            bottom.focusBoundary(false)
                        }
                    })
                }
                sourceComponent: {
                    switch (Chrome.popup) {
                    case "menu":
                        return menuSurface
                    case "save":
                        return saveFailureSurface
                    case "reset":
                        return resetSurface
                    }
                    return null
                }
            }
        }
    }

    Component {
        id: menuSurface
        MenuView {
            editorActive: bottom.menuEditorActive
        }
    }
    Component {
        id: saveFailureSurface
        SaveFailureView {}
    }
    Component {
        id: resetSurface
        ResetSettingsView {}
    }
}
