import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Top chrome content: the continuous Brume ribbon (source, playback output,
// resolution, framerate, window) plus the selector surface.
//
// The ribbon shows the confirmed live selection; every selection is applied
// immediately, so nothing is marked as a pending draft. The native window
// bounds come from chromeWidth/chromeHeight through the root's
// syncChromeGeometry().
Item {
    id: top

    objectName: "topChromeRoot"

    // Driven by the root composition: content fades, then the tool hides.
    property bool chromeShown: true
    property real availableHeight: 524
    readonly property bool compactRibbon: top.width < 660
    opacity: chromeShown ? 1 : 0
    Behavior on opacity {
        NumberAnimation {
            duration: Brume.fadeDuration
            easing: Brume.fadeEasing
        }
    }

    readonly property var profile: Projection.view
    readonly property bool popupActive: Chrome.popupRegion(Chrome.popup) === "top"
    readonly property bool fullscreen: Projection.bridge
        ? Projection.bridge.fullscreenPreference === true : false

    readonly property real chromeWidth: top.width
    readonly property real chromeHeight: ribbon.implicitHeight
        + (top.popupActive ? Brume.gap + topPopup.height : 0)

    // Visible invoker of the open selector surface; focus returns to it when
    // the popup is dismissed.
    property var invoker: null
    property int closingEpoch: -1
    property int surfaceEpoch: -1

    signal extentChanged()
    onChromeWidthChanged: top.extentChanged()
    onChromeHeightChanged: top.extentChanged()

    function reconcilePopup() {
        if (top.popupActive) {
            top.surfaceEpoch = Chrome.popupEpoch
            if (!topPopup.opened) {
                const epoch = top.surfaceEpoch
                Qt.callLater(function() {
                    if (top.popupActive && Chrome.rootAvailable && epoch === Chrome.popupEpoch)
                        topPopup.open()
                })
            }
        } else if (topPopup.visible) {
            topPopup.close()
        }
    }

    // Native entry point: reveal, note keyboard navigation, then focus the
    // boundary control of this tool.
    function focusBoundary(backward) {
        Chrome.noteKeyboardActivity()
        if (top.popupActive) {
            topPopup.focusBoundary(backward)
            return
        }
        const target = backward ? fullscreenButton : sourceControl
        if (target && target.enabled)
            target.forceActiveFocus(Qt.TabFocusReason)
    }

    Connections {
        target: Chrome
        function onPopupChanged() {
            top.reconcilePopup()
        }
    }
    Component.onCompleted: top.reconcilePopup()

    PointerActivity {}

    Timer {
        interval: 2000
        repeat: true
        running: top.chromeShown && top.popupActive && Chrome.rootAvailable
            && (Chrome.popup === "source" || Chrome.popup === "size" || Chrome.popup === "rate")
        onTriggered: Chrome.refreshCatalogs()
    }

    Rectangle {
        x: ribbon.x
        y: ribbon.y
        width: ribbon.width
        height: ribbon.height
        radius: Brume.radiusSurface
        color: Brume.surface
        border.width: Brume.separatorWidth
        border.color: Brume.divider
    }

    GridLayout {
        id: ribbon
        x: 0
        y: 0
        width: parent.width
        enabled: Projection.ready
        implicitHeight: Brume.ribbonHeight * (top.compactRibbon ? 2 : 1)
        height: implicitHeight
        columns: top.compactRibbon ? 3 : 5
        rowSpacing: 0
        columnSpacing: 0

        RibbonControl {
            id: sourceControl
            objectName: "ribbonSource"
            Layout.fillWidth: true
            Layout.minimumWidth: 0
            Layout.preferredWidth: top.width / ribbon.columns
            iconName: "camera"
            caption: "Source"
            value: top.profile.live.sourceLabel.length > 0 ? top.profile.live.sourceLabel : "—"
            Accessible.name: "Source de capture"
            onClicked: {
                top.invoker = sourceControl
                Chrome.openPopup("source")
            }
            Keys.onPressed: function(event) {
                Chrome.noteKeyboardActivity()
                if (event.key === Qt.Key_Backtab) {
                    if (Projection.bridge)
                        Projection.bridge.focusNextChrome("top", true)
                    event.accepted = true
                }
            }
        }

        RibbonControl {
            id: ribbonOutput
            objectName: "ribbonOutput"
            Layout.fillWidth: true
            Layout.minimumWidth: 0
            Layout.preferredWidth: top.width / ribbon.columns
            iconName: "headphones"
            caption: "Audio"
            value: top.profile.output.selectedLabel.length > 0
                ? top.profile.output.selectedLabel : "Auto"
            showChevron: true
            Accessible.name: "Sortie audio de lecture"
            onClicked: {
                top.invoker = ribbonOutput
                Chrome.openPopup("output")
            }
        }

        RibbonControl {
            id: resolutionControl
            objectName: "ribbonResolution"
            Layout.fillWidth: true
            Layout.minimumWidth: 0
            Layout.preferredWidth: top.width / ribbon.columns
            iconName: "maximize"
            caption: "Résolution"
            value: top.profile.live.sizeLabel.length > 0 ? top.profile.live.sizeLabel : "—"
            Accessible.name: "Résolution de capture"
            onClicked: {
                top.invoker = resolutionControl
                Chrome.openPopup("size")
            }
        }

        RibbonControl {
            id: rateControl
            objectName: "ribbonRate"
            Layout.fillWidth: true
            Layout.minimumWidth: 0
            Layout.preferredWidth: top.width / ribbon.columns
            iconName: "refresh"
            caption: "Framerate"
            value: top.profile.live.rateLabel.length > 0
                ? Projection.rateFps(top.profile.live.rateLabel) : "—"
            Accessible.name: "Framerate"
            onClicked: {
                top.invoker = rateControl
                Chrome.openPopup("rate")
            }
        }

        RibbonControl {
            id: fullscreenButton
            objectName: "ribbonFullscreen"
            Layout.fillWidth: true
            Layout.minimumWidth: 0
            Layout.preferredWidth: top.width / ribbon.columns
            iconName: top.fullscreen ? "minimize" : "maximize"
            caption: "Fenêtre"
            value: top.fullscreen ? "Plein écran" : "Fenêtré"
            showChevron: false
            Accessible.name: top.fullscreen ? "Quitter le plein écran" : "Passer en plein écran"
            onClicked: if (Projection.bridge)
                Projection.bridge.toggleFullscreen()
            Keys.onPressed: function(event) {
                Chrome.noteKeyboardActivity()
                if (event.key === Qt.Key_Tab) {
                    if (Projection.bridge)
                        Projection.bridge.focusNextChrome("top", false)
                    event.accepted = true
                }
            }
        }
    }

    BrumePopup {
        id: topPopup
        objectName: "topSelectorPopup"
        x: 0
        y: ribbon.height + Brume.gap
        maximumWidth: top.width
        maximumHeight: Math.max(1, top.availableHeight - ribbon.height - Brume.gap)
        onAboutToHide: top.closingEpoch = top.surfaceEpoch
        onOpened: {
            if (!top.popupActive || !Chrome.rootAvailable || top.surfaceEpoch !== Chrome.popupEpoch)
                return
            top.Window.window.requestActivate()
            topPopup.focusBoundary(false)
        }
        onClosed: {
            const restoreInvoker = Chrome.popupClosed("top", top.closingEpoch)
            if (top.popupActive) {
                // An old exit must not strand its successor with ownership but
                // no visible surface, nor steal the successor's focus.
                top.reconcilePopup()
                return
            }
            if (!restoreInvoker)
                return
            const target = top.invoker ? top.invoker : sourceControl
            if (target && target.visible && target.enabled)
                target.forceActiveFocus(Qt.PopupFocusReason)
        }

        contentItem: BrumeScrollView {
            implicitWidth: topView.implicitWidth
            implicitHeight: topView.implicitHeight
            contentWidth: availableWidth
            contentHeight: topView.implicitHeight
            clip: true
            ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
            ScrollBar.vertical.policy: ScrollBar.AsNeeded
            Loader {
                id: topView
                width: parent.width
                enabled: Projection.ready
                onLoaded: {
                    const epoch = Chrome.popupEpoch
                    Qt.callLater(function() {
                        if (top.popupActive && topPopup.opened && Chrome.rootAvailable
                                && epoch === Chrome.popupEpoch)
                            topPopup.focusBoundary(false)
                    })
                }
                sourceComponent: {
                    switch (Chrome.popup) {
                    case "source":
                        return sourceSelector
                    case "output":
                        return outputSelector
                    case "size":
                        return sizeSelector
                    case "rate":
                        return rateSelector
                    }
                    return null
                }
            }
        }
    }

    Component {
        id: sourceSelector
        SourceSelectorView {}
    }
    Component {
        id: outputSelector
        OutputSelectorView {}
    }
    Component {
        id: sizeSelector
        SizeRateSelectorView { kind: "size" }
    }
    Component {
        id: rateSelector
        SizeRateSelectorView { kind: "rate" }
    }
}
