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

        WindowContainer {
            id: videoContainer
            objectName: "videoContainer"
            Layout.fillWidth: true
            Layout.fillHeight: true
            window: root.bridge.hostWindow
            activeFocusOnTab: true

            Label {
                anchors.centerIn: parent
                visible: root.bridge.hostWindow === null
                text: root.bridge.phase === "Idle" ? (root.bridge.captureSelected ? "Open capture to start" : "Select capture mode on command line") : root.bridge.phase
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
                            + (!root.bridge.audioEnabled && root.bridge.audioSource.length > 0 ? "\nSelected source retained" : "")
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
