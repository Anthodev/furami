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

            ColumnLayout {
                anchors.fill: parent
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
                    enabled: root.bridge.phase === "Idle" && root.bridge.captureSelected
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
                    enabled: root.bridge.phase !== "Idle" && root.bridge.phase !== "QuitReady"
                    onClicked: root.bridge.closeCapture()
                }
                Button {
                    objectName: "closeDuringOpenProof"
                    text: "Close during opening"
                    Layout.fillWidth: true
                    enabled: root.bridge.phase === "Idle" && root.bridge.captureSelected
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
                    text: root.bridge.ended ? "Capture ended. Session stays alive."
                        : root.bridge.paused ? "Playback paused" : "Space pauses / resumes video"
                    wrapMode: Text.WordWrap
                    Layout.fillWidth: true
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
                    text: "Video: click to focus. Space toggles pause. F11 toggles fullscreen. Escape leaves fullscreen or hides panel. Text focus wins."
                    wrapMode: Text.WordWrap
                    Layout.fillWidth: true
                }
                ScrollView {
                    id: diagnosticScroll
                    contentWidth: availableWidth
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    Layout.minimumHeight: 80
                    visible: root.bridge.diagnostic.length > 0
                    Label {
                        objectName: "proofDiagnostic"
                        text: root.bridge.diagnostic
                        width: diagnosticScroll.availableWidth
                        wrapMode: Text.WrapAnywhere
                    }
                }
                Item {
                    Layout.fillHeight: true
                    visible: root.bridge.diagnostic.length === 0
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
