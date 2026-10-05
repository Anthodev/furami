import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

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
    color: palette.window

    // Qt 6.11 Popup.Window also resizes its native window from implicit sizes.
    // Keep those sizes bounded independently of the scrollable message.
    readonly property real settingsDialogWidth: Math.max(1, Math.min(460, root.width - 32,
        (bridge.screenAvailableWidth > 0 ? bridge.screenAvailableWidth : root.width) - 32))
    readonly property real settingsDialogHeight: Math.max(1, Math.min(520, root.height - 32,
        (bridge.screenAvailableHeight > 0 ? bridge.screenAvailableHeight : root.height) - 32))

    onClosing: function(close) {
        close.accepted = bridge.quitAuthorized()
        if (!close.accepted)
            bridge.requestApplicationClose()
    }

    function reconcilePlaybackControls() {
        playbackVolume.value = Qt.binding(function() { return root.bridge.volumePercent })
        playbackMute.checked = Qt.binding(function() { return root.bridge.muted })
    }
    function reconcileSettingsDialogs() {
        dirtyCloseDialog.visible = bridge.closeDialog === "draft"
        saveFailureDialog.visible = bridge.closeDialog === "save" || bridge.closeDialog === "warning"
        resetSettingsDialog.visible = bridge.resetToken !== "0"
        bridge.popupOpen = panelPopup.visible || dirtyCloseDialog.visible
            || saveFailureDialog.visible || resetSettingsDialog.visible
    }

    Connections {
        target: root.bridge
        function onStateChanged() {
            root.reconcilePlaybackControls()
            root.reconcileSettingsDialogs()
        }
    }
    Component.onCompleted: reconcileSettingsDialogs()

    header: ToolBar {
        RowLayout {
            anchors.fill: parent
            anchors.leftMargin: 12
            anchors.rightMargin: 12
            spacing: 12

            Label {
                text: "Furami"
                font.bold: true
            }
            Label {
                text: root.bridge.phase === "WaitingSurface" ? "Opening" : root.bridge.phase
                Layout.fillWidth: true
            }
            Button {
                objectName: "togglePanelProof"
                text: root.bridge.panelVisible ? "Hide panel" : "Show panel"
                enabled: !root.bridge.popupOpen
                onClicked: root.bridge.togglePanel()
            }
            Button {
                objectName: "fullscreenProof"
                text: root.visibility === Window.FullScreen ? "Leave fullscreen" : "Fullscreen"
                onClicked: root.bridge.toggleFullscreen()
                enabled: !root.bridge.closing
            }
        }
    }

    RowLayout {
        anchors.fill: parent
        spacing: 0

        Item {
            Layout.fillWidth: true
            Layout.fillHeight: true

            WindowContainer {
                id: videoContainer
                objectName: "videoContainer"
                anchors.fill: parent
                window: root.bridge.hostWindow
                activeFocusOnTab: true
                // Native windows stack above Qt Quick Items: a Rectangle
                // cannot conceal them. Visibility hides without releasing
                // the permanent container or its owner's native surface.
                visible: !root.bridge.presentationSuppressed
                    && root.bridge.productPhase !== "Disconnected"
            }
            Label {
                anchors.centerIn: parent
                visible: root.bridge.hostWindow === null
                text: root.bridge.phase === "Idle" ? (root.bridge.captureSelected ? "Open capture to start" : "Select capture mode on command line") : root.bridge.phase
            }
            Rectangle {
                anchors.fill: parent
                visible: root.bridge.productPhase === "Disconnected"
                    || (root.bridge.presentationSuppressed && root.bridge.hostWindow !== null)
                color: palette.window
                Label {
                    anchors.centerIn: parent
                    width: Math.min(parent.width - 32, 420)
                    horizontalAlignment: Text.AlignHCenter
                    wrapMode: Text.WordWrap
                    text: root.bridge.productPhase === "Disconnected"
                        ? "Capture disconnected"
                            + (root.bridge.recoveryEvidence.length > 0 ? "\n" + root.bridge.recoveryEvidence : "")
                        : "Capture hidden until Resume"
                }
            }
        }

        Pane {
            id: panel
            objectName: "proofPanel"
            visible: root.bridge.panelVisible
            Layout.preferredWidth: 240
            Layout.minimumWidth: 240
            Layout.maximumWidth: 240
            Layout.fillHeight: true
            Layout.minimumHeight: 0
            implicitHeight: 0
            padding: 12

            ScrollView {
                id: panelScroll
                anchors.fill: parent
                // Content height must not become the pane's layout minimum.
                implicitWidth: 0
                implicitHeight: 0
                clip: true
                contentWidth: availableWidth
                contentHeight: panelContent.implicitHeight
                activeFocusOnTab: true
                ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
                ScrollBar.vertical.policy: ScrollBar.AsNeeded

                function revealFocusedControl() {
                    const item = root.activeFocusItem
                    let ancestor = item
                    while (ancestor && ancestor !== panelContent)
                        ancestor = ancestor.parent
                    if (!ancestor)
                        return
                    const viewport = panelScroll.contentItem
                    const top = item.mapToItem(panelContent, 0, 0).y
                    const bottom = top + item.height
                    let position = viewport.contentY
                    if (top < position)
                        position = top
                    else if (bottom > position + viewport.height)
                        position = bottom - viewport.height
                    viewport.contentY = Math.max(0, Math.min(position,
                        Math.max(0, panelScroll.contentHeight - viewport.height)))
                }

                Connections {
                    target: root
                    function onActiveFocusItemChanged() {
                        panelScroll.revealFocusedControl()
                    }
                }

                ColumnLayout {
                    id: panelContent
                    width: Math.max(0, panelScroll.availableWidth - panelScroll.effectiveScrollBarWidth)
                    spacing: 8

                    Label {
                        text: "Selected capture / Vulkan"
                        font.bold: true
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    Label {
                        objectName: "savedSelection"
                        visible: root.bridge.savedSelection.length > 0
                        text: root.bridge.savedSelection
                            + (root.bridge.draftDirty ? "\nUnapplied selection changes" : "\nSelection unchanged")
                        textFormat: Text.PlainText
                        wrapMode: Text.WrapAnywhere
                        Layout.fillWidth: true
                        Accessible.name: "Capture selection and exact mode"
                    }
                    Label {
                        objectName: "startupRestoreReason"
                        text: root.bridge.startupReason
                        textFormat: Text.PlainText
                        visible: text.length > 0
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                        Accessible.name: "Startup restoration status"
                    }
                    Label {
                        objectName: "settingsStatus"
                        text: root.bridge.settingsStatus
                        textFormat: Text.PlainText
                        wrapMode: Text.WrapAnywhere
                        Layout.fillWidth: true
                        Accessible.name: "Settings persistence status"
                    }
                    Label {
                        objectName: "settingsPath"
                        text: root.bridge.settingsPath
                        textFormat: Text.PlainText
                        visible: text.length > 0
                        wrapMode: Text.WrapAnywhere
                        Layout.fillWidth: true
                        Accessible.name: "Settings file path"
                    }
                    Button {
                        objectName: "resetSettings"
                        text: "Reset saved file…"
                        enabled: !root.bridge.closing && root.bridge.settingsPath.length > 0
                        Layout.fillWidth: true
                        Accessible.name: "Reset the saved settings file with confirmation"
                        onClicked: root.bridge.requestSettingsReset()
                    }
                    Button {
                        objectName: "openProof"
                        text: "Open capture"
                        Layout.fillWidth: true
                        enabled: root.bridge.canOpen
                        onClicked: root.bridge.openCapture()
                    }
                    Button {
                        objectName: "restartCapture"
                        text: "Restart capture"
                        Layout.fillWidth: true
                        enabled: root.bridge.canRestart
                        onClicked: root.bridge.restartCapture(root.bridge.restartGeneration)
                    }
                    // Reconnect stays visible in every session state: a healthy
                    // session no-ops, a pending loss joins, and a shutdown is
                    // rejected with the coordinator's actionable reason.
                    Button {
                        objectName: "reconnectCapture"
                        text: "Reconnect capture"
                        Layout.fillWidth: true
                        enabled: !root.bridge.closing
                        onClicked: root.bridge.reconnectCapture(root.bridge.restartGeneration)
                    }
                    Button {
                        objectName: "closeProof"
                        text: "Close session"
                        Layout.fillWidth: true
                        enabled: !root.bridge.closing && root.bridge.captureSelected
                            && root.bridge.phase !== "QuitReady"
                            && (root.bridge.phase !== "Idle" || !root.bridge.canOpen)
                        onClicked: root.bridge.closeCapture()
                    }
                    Button {
                        objectName: "closeDuringOpenProof"
                        text: "Close during opening"
                        Layout.fillWidth: true
                        enabled: root.bridge.canOpen
                        onClicked: root.bridge.openAndCloseDuringOpeningForProof()
                    }
                    Button {
                        objectName: "forceSurfaceLossProof"
                        text: "Force surface loss"
                        Layout.fillWidth: true
                        enabled: root.bridge.phase === "Ready"
                        onClicked: root.bridge.forceSurfaceLossForProof()
                    }
                    Label {
                        text: "Session: " + root.bridge.productPhase
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    Label {
                        objectName: "recoveryStatus"
                        visible: root.bridge.recoveryEvidence.length > 0
                            || root.bridge.recoveryStage.length > 0
                        text: (root.bridge.recoveryEvidence.length > 0
                                ? root.bridge.recoveryEvidence + "\n" : "")
                            + root.bridge.recoveryStage
                        wrapMode: Text.WrapAnywhere
                        Layout.fillWidth: true
                        Accessible.name: "Capture recovery status"
                    }
                    Repeater {
                        model: root.bridge.recoveryCandidates.length > 0
                            ? root.bridge.recoveryCandidates.split("\n") : 0
                        ColumnLayout {
                            id: recoveryChoice
                            required property string modelData
                            readonly property var fields: modelData.split("|")
                            Layout.fillWidth: true
                            spacing: 2
                            Label {
                                text: (recoveryChoice.fields.length > 1
                                        ? recoveryChoice.fields[1] : recoveryChoice.modelData)
                                    + (recoveryChoice.fields.length > 2
                                        ? "  " + recoveryChoice.fields[2] : "")
                                wrapMode: Text.WrapAnywhere
                                Layout.fillWidth: true
                            }
                            Label {
                                visible: recoveryChoice.fields.length > 4
                                text: "bus " + recoveryChoice.fields[3]
                                    + " ports " + recoveryChoice.fields[4]
                                    + (recoveryChoice.fields.length > 5
                                        && recoveryChoice.fields[5] !== "-"
                                        ? " serial " + recoveryChoice.fields[5] : "")
                                wrapMode: Text.WrapAnywhere
                                Layout.fillWidth: true
                            }
                            Button {
                                objectName: "chooseRecovery"
                                text: "Use this source"
                                Layout.fillWidth: true
                                enabled: root.bridge.captureSelected
                                Accessible.name: "Reconnect with this capture source"
                                onClicked: root.bridge.chooseRecovery(
                                    root.bridge.restartGeneration,
                                    recoveryChoice.modelData)
                            }
                        }
                    }
                    Label {
                        text: "Playback: " + root.bridge.playbackStatus
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                        Accessible.name: "Playback status"
                    }
                    Button {
                        objectName: "togglePlayback"
                        text: root.bridge.paused ? "Resume" : "Pause"
                        enabled: root.bridge.canTogglePause
                        Layout.fillWidth: true
                        Accessible.name: root.bridge.paused ? "Resume live capture" : "Pause live capture"
                        onClicked: root.bridge.togglePause()
                    }
                    Label {
                        text: "Volume: " + root.bridge.volumePercent + "%"
                        Layout.fillWidth: true
                    }
                    Slider {
                        id: playbackVolume
                        objectName: "playbackVolume"
                        from: 0
                        to: 100
                        stepSize: 1
                        value: root.bridge.volumePercent
                        enabled: root.bridge.canSetGain
                        Layout.fillWidth: true
                        Accessible.name: "Playback volume"
                        onMoved: {
                            root.bridge.setVolume(Math.round(value))
                            root.reconcilePlaybackControls()
                        }
                    }
                    Button {
                        id: playbackMute
                        objectName: "playbackMute"
                        checkable: true
                        checked: root.bridge.muted
                        text: root.bridge.muted ? "Unmute" : "Mute"
                        enabled: root.bridge.canSetGain
                        Layout.fillWidth: true
                        Accessible.name: root.bridge.muted ? "Unmute playback audio" : "Mute playback audio"
                        onClicked: {
                            root.bridge.setMuted(checked)
                            root.reconcilePlaybackControls()
                        }
                    }
                    Label {
                        text: "Audio: " + root.bridge.audioStatus
                            + (root.bridge.audioSource.length > 0 ? "\n" + root.bridge.audioSource : "")
                            + (root.bridge.audioStatus === "Disabled" && root.bridge.audioDesired.length > 0
                                ? "\nAudio off — selected source retained: " + root.bridge.audioDesired : "")
                            + (root.bridge.audioDiagnostic.length > 0 ? "\n" + root.bridge.audioDiagnostic : "")
                        textFormat: Text.PlainText
                        wrapMode: Text.WrapAnywhere
                        Layout.fillWidth: true
                        Accessible.name: "Capture audio status"
                    }
                    TextField {
                        id: proofText
                        objectName: "proofText"
                        Accessible.name: "Text input precedence proof"
                        placeholderText: "Type here to test focus"
                        Layout.fillWidth: true
                        selectByMouse: true
                        onActiveFocusChanged: root.bridge.textEntryActive = activeFocus
                    }
                    Button {
                        id: popupButton
                        objectName: "popupOpenProof"
                        text: "Panel-only popup"
                        Layout.fillWidth: true
                        onClicked: panelPopup.open()
                    }
                    Label {
                        text: "Video: click to focus. Space toggles pause. F11 toggles fullscreen. Escape leaves fullscreen or hides panel. Controls keep Space."
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    Label {
                        objectName: "proofDiagnostic"
                        text: root.bridge.diagnostic
                        textFormat: Text.PlainText
                        visible: root.bridge.diagnostic.length > 0
                        wrapMode: Text.WrapAnywhere
                        Layout.fillWidth: true
                        Accessible.name: "Capture diagnostics"
                    }
                }
            }

            Popup {
                id: panelPopup
                objectName: "panelPopupProof"
                parent: panel
                popupType: Popup.Item
                x: 12
                y: Math.max(12, Math.min(panel.height - height - 12,
                    popupButton.mapToItem(panel, 0, popupButton.height).y + 8))
                width: panel.width - 24
                height: 144
                modal: false
                dim: false
                focus: true
                closePolicy: Popup.CloseOnEscape | Popup.CloseOnPressOutsideParent
                onVisibleChanged: root.reconcileSettingsDialogs()
                onClosed: popupButton.forceActiveFocus(Qt.TabFocusReason)

                contentItem: ColumnLayout {
                    spacing: 8
                    Label {
                        text: "This popup stays inside the panel. No scrim crosses video."
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    Button {
                        objectName: "closePopupProof"
                        text: "Close popup"
                        Layout.fillWidth: true
                        onClicked: panelPopup.close()
                    }
                }
            }
        }
    }
    footer: ToolBar {
        visible: root.bridge.settingsRefused || root.bridge.settingsPath.length === 0
        Label {
            objectName: "settingsRefusalBanner"
            width: parent.width
            padding: 12
            text: root.bridge.settingsRefused
                ? "Settings file refused. Changes are session-only until a confirmed reset. Details in the panel."
                : "Settings location unavailable. Changes are session-only. Details in the panel."
            textFormat: Text.PlainText
            wrapMode: Text.WordWrap
            Accessible.name: "Settings not persistent"
        }
    }

    // Native capture children stack above Item popups. Real modal Qt windows
    // keep confirmation visible and focusable without replacing the video host.
    Dialog {
        id: dirtyCloseDialog
        objectName: "dirtyCloseDialog"
        popupType: Popup.Window
        title: "Unapplied selection changes"
        modal: true
        dim: false
        focus: true
        implicitWidth: root.settingsDialogWidth
        implicitHeight: root.settingsDialogHeight
        width: implicitWidth
        height: implicitHeight
        // Use the root overlay, not the popup window's attached overlay.
        // Explicit coordinates also retain Qt's native QScreen fitting step.
        parent: root.Overlay.overlay
        x: Math.round((parent.width - width) / 2)
        y: Math.round((parent.height - height) / 2)
        closePolicy: Popup.NoAutoClose
        property string expectedRevision: "0"
        onOpened: {
            expectedRevision = root.bridge.closeRevision
            cancelDirtyClose.forceActiveFocus(Qt.TabFocusReason)
        }
        onRejected: root.bridge.decideClose(false, expectedRevision)
        onClosed: {
            if (root.bridge.closeDialog === "draft")
                root.bridge.decideClose(false, expectedRevision)
        }
        header: Label {
            text: dirtyCloseDialog.title
            textFormat: Text.PlainText
            wrapMode: Text.WrapAnywhere
            font.bold: true
            padding: 12
        }
        // A plain Item isolates viewport sizing from the full message height.
        contentItem: Item {
            implicitWidth: 0
            implicitHeight: 0
            ScrollView {
                id: dirtyCloseContent
                objectName: "dirtyCloseContent"
                anchors.fill: parent
                implicitWidth: 0
                implicitHeight: 0
                clip: true
                activeFocusOnTab: true
                contentWidth: availableWidth
                contentHeight: dirtyCloseMessage.implicitHeight
                ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
                ScrollBar.vertical.policy: ScrollBar.AsNeeded
                Keys.onEscapePressed: dirtyCloseDialog.reject()
                ColumnLayout {
                    id: dirtyCloseMessage
                    width: Math.max(0, dirtyCloseContent.availableWidth - dirtyCloseContent.effectiveScrollBarWidth)
                    spacing: 12
                    Label {
                        text: "The selection has changes that were never applied. Quit without applying them? Only the last saved/applied capture and your current local preferences may be saved."
                        wrapMode: Text.WrapAnywhere
                        Layout.fillWidth: true
                        Accessible.name: text
                    }
                    Label {
                        text: root.bridge.savedSelection
                        textFormat: Text.PlainText
                        wrapMode: Text.WrapAnywhere
                        Layout.fillWidth: true
                    }
                }
            }
        }
        footer: Pane {
            padding: 12
            Keys.onEscapePressed: dirtyCloseDialog.reject()
            contentItem: ColumnLayout {
                spacing: 8
                Button {
                    id: cancelDirtyClose
                    objectName: "cancelDirtyClose"
                    text: "Cancel close"
                    Layout.fillWidth: true
                    onClicked: dirtyCloseDialog.reject()
                }
                Button {
                    objectName: "discardDirtyClose"
                    text: "Quit without applying"
                    Layout.fillWidth: true
                    onClicked: {
                        root.bridge.decideClose(true, dirtyCloseDialog.expectedRevision)
                        // A stale answer vetoes close; the next decision uses
                        // the refreshed displayed selection and revision.
                        dirtyCloseDialog.expectedRevision = root.bridge.closeRevision
                    }
                }
            }
        }
    }

    Dialog {
        id: saveFailureDialog
        objectName: "saveFailureDialog"
        popupType: Popup.Window
        title: root.bridge.closeDialog === "warning" ? "Durability not confirmed" : "Unable to save settings"
        modal: true
        dim: false
        focus: true
        implicitWidth: root.settingsDialogWidth
        implicitHeight: root.settingsDialogHeight
        width: implicitWidth
        height: implicitHeight
        parent: root.Overlay.overlay
        x: Math.round((parent.width - width) / 2)
        y: Math.round((parent.height - height) / 2)
        closePolicy: Popup.NoAutoClose
        onOpened: {
            if (root.bridge.closeDialog === "warning")
                closeWithoutSaving.forceActiveFocus(Qt.TabFocusReason)
            else
                retrySettingsSave.forceActiveFocus(Qt.TabFocusReason)
        }
        onClosed: {
            if (root.bridge.closeDialog === "save" || root.bridge.closeDialog === "warning")
                Qt.callLater(root.reconcileSettingsDialogs)
        }
        header: Label {
            text: saveFailureDialog.title
            textFormat: Text.PlainText
            wrapMode: Text.WrapAnywhere
            font.bold: true
            padding: 12
        }
        contentItem: Item {
            implicitWidth: 0
            implicitHeight: 0
            ScrollView {
                id: saveFailureContent
                objectName: "saveFailureContent"
                anchors.fill: parent
                implicitWidth: 0
                implicitHeight: 0
                clip: true
                activeFocusOnTab: true
                contentWidth: availableWidth
                contentHeight: saveFailureMessage.implicitHeight
                ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
                ScrollBar.vertical.policy: ScrollBar.AsNeeded
                Label {
                    id: saveFailureMessage
                    width: Math.max(0, saveFailureContent.availableWidth - saveFailureContent.effectiveScrollBarWidth)
                    text: "Capture teardown is complete. No media owner will reopen.\n\n" + root.bridge.settingsStatus
                    textFormat: Text.PlainText
                    wrapMode: Text.WrapAnywhere
                    Accessible.name: text
                }
            }
        }
        footer: Pane {
            padding: 12
            contentItem: ColumnLayout {
                spacing: 8
                Button {
                    id: retrySettingsSave
                    objectName: "retrySettingsSave"
                    text: "Retry save"
                    visible: root.bridge.closeDialog === "save"
                    Layout.fillWidth: true
                    onClicked: root.bridge.retrySave()
                }
                Button {
                    id: closeWithoutSaving
                    objectName: "closeWithoutSaving"
                    text: root.bridge.closeDialog === "warning" ? "Close with durability unconfirmed" : "Close without saving"
                    Layout.fillWidth: true
                    onClicked: root.bridge.closeWithoutSave()
                }
            }
        }
    }

    Dialog {
        id: resetSettingsDialog
        objectName: "resetSettingsDialog"
        popupType: Popup.Window
        title: "Replace the saved settings file?"
        modal: true
        dim: false
        focus: true
        implicitWidth: root.settingsDialogWidth
        implicitHeight: root.settingsDialogHeight
        width: implicitWidth
        height: implicitHeight
        parent: root.Overlay.overlay
        x: Math.round((parent.width - width) / 2)
        y: Math.round((parent.height - height) / 2)
        closePolicy: Popup.NoAutoClose
        property string confirmationToken: "0"
        onOpened: {
            confirmationToken = root.bridge.resetToken
            cancelSettingsReset.forceActiveFocus(Qt.TabFocusReason)
        }
        onRejected: root.bridge.decideSettingsReset(confirmationToken, false)
        onClosed: {
            if (root.bridge.resetToken === confirmationToken)
                root.bridge.decideSettingsReset(confirmationToken, false)
        }
        header: Label {
            text: resetSettingsDialog.title
            textFormat: Text.PlainText
            wrapMode: Text.WrapAnywhere
            font.bold: true
            padding: 12
        }
        contentItem: Item {
            implicitWidth: 0
            implicitHeight: 0
            ScrollView {
                id: resetSettingsContent
                objectName: "resetSettingsContent"
                anchors.fill: parent
                implicitWidth: 0
                implicitHeight: 0
                clip: true
                activeFocusOnTab: true
                contentWidth: availableWidth
                contentHeight: resetSettingsMessage.implicitHeight
                ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
                ScrollBar.vertical.policy: ScrollBar.AsNeeded
                Keys.onEscapePressed: resetSettingsDialog.reject()
                Label {
                    id: resetSettingsMessage
                    width: Math.max(0, resetSettingsContent.availableWidth - resetSettingsContent.effectiveScrollBarWidth)
                    text: "The original file will be replaced and its contents lost:\n" + root.bridge.settingsPath
                        + "\n\nThe new file saves your current volume, mute and fullscreen choice, with no saved capture. This does not stop live capture, apply or change the draft, or reset live preferences."
                    textFormat: Text.PlainText
                    wrapMode: Text.WrapAnywhere
                    Accessible.name: text
                }
            }
        }
        footer: Pane {
            padding: 12
            Keys.onEscapePressed: resetSettingsDialog.reject()
            contentItem: ColumnLayout {
                spacing: 8
                Button {
                    id: cancelSettingsReset
                    objectName: "cancelSettingsReset"
                    text: "Cancel reset"
                    Layout.fillWidth: true
                    onClicked: resetSettingsDialog.reject()
                }
                Button {
                    objectName: "confirmSettingsReset"
                    text: "Replace original file"
                    Layout.fillWidth: true
                    onClicked: root.bridge.decideSettingsReset(resetSettingsDialog.confirmationToken, true)
                }
            }
        }
    }
}
