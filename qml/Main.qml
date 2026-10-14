import QtQuick
import QtQuick.Controls
import dev.antho.furami

// Furami main window and chrome composition (FUR-029, Brume).
//
// The permanent native video container keeps its original host binding and the
// full viewport. The chrome lives in exactly three bounded, frameless,
// transient Qt.Tool windows whose content is the Brume ribbon, volume rail and
// bottom menu. The video host is never reparented and no frame is copied.
ApplicationWindow {
    id: root
    objectName: "furamiRoot"
    required property FuramiBridge bridge
    visible: true
    title: "Furami"
    width: 960
    height: 600
    minimumWidth: 480
    minimumHeight: 540
    color: Brume.window

    readonly property var profile: Projection.view
    // Chrome maps only while the Furami window family owns the foreground
    // focus. This single outer gate covers all three tool windows, their
    // opacity fade tails and toolVisible.
    readonly property bool chromeAvailable: root.visible
        && root.visibility !== Window.Minimized
        && root.bridge.chromeFamilyActive === true
    readonly property string chromeFocusOwner: {
        const windows = [topChromeWindow, volumeChromeWindow, bottomChromeWindow]
        const contents = [topChromeContent, volumeChromeContent, bottomChromeContent]
        const regions = ["top", "volume", "bottom"]
        for (let i = 0; i < windows.length; ++i) {
            const window = windows[i]
            const item = window.activeFocusItem
            if (window.visible && window.active && item && item.activeFocus
                    && item.visible && item.enabled
                    && item !== window.contentItem && item !== contents[i])
                return regions[i]
        }
        return ""
    }
    onChromeFocusOwnerChanged: {
        if (root.chromeFocusOwner !== "")
            Chrome.setChromeFocus(root.chromeFocusOwner, true)
        else
            Chrome.setChromeFocus(Chrome.focusOwner, false)
    }
    readonly property bool videoShowing: Projection.ready
        && root.bridge.hostWindow !== null && !root.bridge.presentationSuppressed
        && root.profile.productPhase !== "Disconnected"

    // The producer emits a null selection for an empty or genuinely incomplete
    // bootstrap, so `selection.present` already means a complete capture tuple.
    readonly property bool sessionReady: root.profile.applied.present
        || root.profile.lastValid.present
        || root.profile.selection.present

    readonly property string backdropTitle: {
        if (Projection.failureText.length > 0)
            return "Interface indisponible"
        if (!Projection.ready)
            return "Préparation de l'interface…"
        if (root.profile.productPhase === "Disconnected")
            return "Capture déconnectée"
        if (root.profile.opening)
            return "Ouverture de la capture…"
        if (root.bridge.presentationSuppressed)
            return "Capture masquée jusqu'à la reprise"
        if (!root.sessionReady)
            return "Aucune capture sélectionnée"
        return Projection.productPhaseText(root.profile.productPhase)
    }

    readonly property string backdropDetail: {
        if (Projection.failureText.length > 0)
            return Projection.failureText
        if (!Projection.ready)
            return ""
        if (root.profile.productPhase === "Disconnected")
            return root.profile.recovery.evidence.length > 0
                ? root.profile.recovery.evidence
                : "Utilisez Options pour reconnecter une source."
        if (root.profile.opening)
            return "Validation et ouverture de la source en cours."
        if (root.bridge.presentationSuppressed)
            return "La capture reprendra à la reprise demandée."
        if (!root.sessionReady)
            return "Choisissez une source dans Options : la capture s'ouvre immédiatement."
        return ""
    }

    readonly property string backdropDiagnostic: root.bridge.diagnostic

    property bool syncPending: false

    // ── Native-facing hooks ───────────────────────────────────────────────
    function syncChromeGeometry() {
        const margin = Brume.windowMargin
        const availableWidth = Math.max(1, root.width - 2 * margin)

        if (!root.visible || root.visibility === Window.Minimized) {
            root.hideChromeWindows()
            return
        }

        // Top ribbon: full viewport width minus margins, height only the
        // ribbon extent (plus the open selector surface).
        topChromeWindow.width = availableWidth
        topChromeWindow.height = Math.max(1, Math.min(root.height - 2 * margin,
            topChromeContent.chromeHeight))
        root.moveTool(topChromeWindow, margin, margin)

        const volumeWidth = Math.max(1, Math.min(volumeChromeContent.chromeWidth, availableWidth))
        volumeChromeWindow.width = volumeWidth
        volumeChromeWindow.height = Math.max(1, volumeChromeContent.chromeHeight)
        root.moveTool(volumeChromeWindow,
            root.width - margin - volumeWidth,
            (root.height - volumeChromeContent.chromeHeight) / 2)

        // Bottom tool: full available width so the session context and stream
        // statistics stay anchored bottom-left while Options stays bottom-right.
        // Only its bounded height depends on the content and open surface.
        bottomChromeWindow.width = availableWidth
        bottomChromeWindow.height = Math.max(1, Math.min(root.height - 2 * margin,
            bottomChromeContent.chromeHeight))
        root.moveTool(bottomChromeWindow,
            margin,
            root.height - margin - bottomChromeWindow.height)
    }

    // Return HANDLED, not necessarily dismissed: mandatory decisions consume
    // Escape/outside presses and re-activate their real actionable surface.
    function dismissChromePopup() {
        const handled = Chrome.dismissPopup()
        if (Chrome.mandatoryPopupOpen && root.chromeAvailable) {
            bottomChromeWindow.requestActivate()
            bottomChromeContent.focusBoundary(false)
        }
        return handled
    }

    function moveTool(window, localX, localY) {
        const point = root.contentItem.mapToGlobal(localX, localY)
        window.x = Math.round(point.x)
        window.y = Math.round(point.y)
    }

    function scheduleSync() {
        if (root.syncPending)
            return
        root.syncPending = true
        Qt.callLater(function() {
            root.syncPending = false
            root.syncChromeGeometry()
        })
    }

    function toolVisible(region) {
        // The activity counter is part of the dependency set on purpose: if a
        // tool was hidden by the window manager, the next real activity must
        // re-assert its visibility even though the mode was already "shown".
        const activity = Chrome.activityCounter
        if (activity < 0)
            return false
        if (!root.chromeAvailable)
            return false
        if (!Chrome.chromeShowing)
            return false
        if (!Chrome.popupOpen)
            return true
        return Chrome.popupRegion(Chrome.popup) === region
    }

    function hideChromeWindows() {
        Chrome.dismissForRootHide()
    }

    function registerChromeWindows() {
        if (root.bridge && typeof root.bridge.registerChromeWindows === "function")
            root.bridge.registerChromeWindows(topChromeWindow, volumeChromeWindow, bottomChromeWindow)
        root.syncChromeGeometry()
        Chrome.reconcileVisibility()
    }

    onClosing: function(close) {
        if (root.bridge.quitAuthorized()) {
            close.accepted = true
            return
        }
        close.accepted = false
        root.bridge.requestApplicationClose()
    }

    onXChanged: root.scheduleSync()
    onYChanged: root.scheduleSync()
    onWidthChanged: root.scheduleSync()
    onHeightChanged: root.scheduleSync()
    onScreenChanged: root.scheduleSync()
    onVisibilityChanged: {
        if (root.visibility === Window.Minimized || !root.visible)
            root.hideChromeWindows()
        else {
            root.scheduleSync()
            bottomChromeContent.syncSessionDialogs()
            Chrome.reconcileVisibility()
        }
    }

    Component.onCompleted: {
        Projection.bridge = root.bridge
        Chrome.bridge = root.bridge
        Qt.callLater(root.registerChromeWindows)
        Chrome.rootAvailable = Qt.binding(function() { return root.chromeAvailable })
    }

    Connections {
        target: root.bridge
        function onPointerActivity() {
            Chrome.notePointerActivity()
        }
        function onStateChanged() {
            root.scheduleSync()
        }
    }

    // The runtime projects its own popup state; native key routing still reads
    // the legacy scalar while the snapshot remains authoritative for QML.
    Binding {
        target: root.bridge
        property: "popupOpen"
        value: Chrome.popupOpen
    }
    Binding {
        target: root.bridge
        property: "textEntryActive"
        value: Chrome.textEntryActive
    }

    TapHandler {
        onTapped: root.dismissChromePopup()
    }
    // ── Permanent native video host ───────────────────────────────────────
    WindowContainer {
        id: videoContainer
        objectName: "videoContainer"
        anchors.fill: parent
        window: root.bridge.hostWindow
        activeFocusOnTab: true
        // Native windows stack above Qt Quick items; this only hides the host,
        // never releasing the permanent container or its native surface.
        visible: root.videoShowing
    }

    Rectangle {
        id: backdrop
        anchors.fill: parent
        visible: !root.videoShowing
        color: Brume.window

        Column {
            anchors.centerIn: parent
            width: Math.min(parent.width - 2 * Brume.edge * 2, 460)
            spacing: Brume.gap

            BrumeLabel {
                objectName: "backdropTitle"
                width: parent.width
                horizontalAlignment: Text.AlignHCenter
                text: root.backdropTitle
                variant: "title"
            }
            BrumeLabel {
                objectName: "backdropDetail"
                width: parent.width
                horizontalAlignment: Text.AlignHCenter
                visible: text.length > 0
                variant: "secondary"
                text: root.backdropDetail
            }
            BrumeLabel {
                objectName: "backdropDiagnostic"
                width: parent.width
                horizontalAlignment: Text.AlignHCenter
                visible: text.length > 0
                variant: "note"
                text: root.backdropDiagnostic
            }
        }
    }

    // ── Chrome tools ──────────────────────────────────────────────────────
    Window {
        id: topChromeWindow
        objectName: "topChromeWindow"
        flags: Qt.Tool | Qt.FramelessWindowHint
        transientParent: root
        color: "transparent"
        visible: root.chromeAvailable
            && (!Chrome.popupOpen || Chrome.popupRegion(Chrome.popup) === "top")
            && (root.toolVisible("top") || topChromeContent.opacity > 0)

        TopChromeContent {
            id: topChromeContent
            anchors.fill: parent
            chromeShown: root.toolVisible("top")
            availableHeight: root.height - 2 * Brume.windowMargin
            onExtentChanged: root.scheduleSync()
        }
    }

    Window {
        id: volumeChromeWindow
        objectName: "volumeChromeWindow"
        flags: Qt.Tool | Qt.FramelessWindowHint
        transientParent: root
        color: "transparent"
        visible: root.chromeAvailable
            && !Chrome.popupOpen
            && (root.toolVisible("volume") || volumeChromeContent.opacity > 0)

        VolumeChromeContent {
            id: volumeChromeContent
            anchors.fill: parent
            chromeShown: root.toolVisible("volume")
            onExtentChanged: root.scheduleSync()
        }
    }

    Window {
        id: bottomChromeWindow
        objectName: "bottomChromeWindow"
        flags: Qt.Tool | Qt.FramelessWindowHint
        transientParent: root
        color: "transparent"
        visible: root.chromeAvailable
            && (!Chrome.popupOpen || Chrome.popupRegion(Chrome.popup) === "bottom")
            && (root.toolVisible("bottom") || bottomChromeContent.opacity > 0)

        BottomChromeContent {
            id: bottomChromeContent
            anchors.fill: parent
            chromeShown: root.toolVisible("bottom")
            availableHeight: root.height - 2 * Brume.windowMargin
            onExtentChanged: root.scheduleSync()
        }
    }
}
