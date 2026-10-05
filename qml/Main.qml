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

    onClosing: function(close) {
        close.accepted = bridge.quitAuthorized()
        if (!close.accepted)
            bridge.requestApplicationClose()
    }

    function reconcilePlaybackControls() {
        playbackVolume.value = Qt.binding(function() { return root.bridge.volumePercent })
        playbackMute.checked = Qt.binding(function() { return root.bridge.muted })
    }

    Connections {
        target: root.bridge
        function onStateChanged() {
            root.reconcilePlaybackControls()
        }
    }

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
            padding: 12

            ScrollView {
                id: panelScroll
                anchors.fill: parent
                contentWidth: availableWidth
                ScrollBar.horizontal.policy: ScrollBar.AlwaysOff
                ScrollBar.vertical.policy: ScrollBar.AsNeeded

                ColumnLayout {
                    width: panelScroll.availableWidth
                    spacing: 8

                    Label {
                        text: "Selected capture / Vulkan"
                        font.bold: true
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
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
                        enabled: root.bridge.phase !== "QuitReady"
                        onClicked: root.bridge.reconnectCapture(root.bridge.restartGeneration)
                    }
                    Button {
                        objectName: "closeProof"
                        text: "Close session"
                        Layout.fillWidth: true
                        enabled: root.bridge.captureSelected && root.bridge.phase !== "QuitReady"
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
                onVisibleChanged: root.bridge.popupOpen = visible
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
}
