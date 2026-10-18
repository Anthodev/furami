import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Right-centre volume chrome: gain rail plus independent mute.
//
// The tool is narrow; it only grows in height while the pointer hovers or a
// control holds keyboard focus. Hovering adjusts the extent only and is never
// reported as activity, so a stationary cursor still lets the chrome fade.
Item {
    id: volume

    objectName: "volumeChromeRoot"

    // Driven by the root composition: content fades, then the tool hides.
    property bool chromeShown: true
    opacity: chromeShown ? 1 : 0
    Behavior on opacity {
        NumberAnimation {
            duration: Brume.fadeDuration
            easing: Brume.fadeEasing
        }
    }

    readonly property var profile: Projection.view
    readonly property bool hovered: extentHandler.hovered
    readonly property bool focused: volumeSlider.activeFocus || muteButton.activeFocus
    property bool keyboardExpanded: false
    readonly property bool expanded: hovered || focused || keyboardExpanded

    readonly property real chromeWidth: Brume.volumeWidth
    readonly property real chromeHeight: expanded ? 216 : Brume.ribbonHeight

    signal extentChanged()
    onChromeWidthChanged: volume.extentChanged()
    onChromeHeightChanged: volume.extentChanged()

    function focusBoundary(backward) {
        volume.keyboardExpanded = true
        Chrome.noteKeyboardActivity()
        const target = backward ? muteButton : volumeSlider
        if (target && target.enabled)
            target.forceActiveFocus(Qt.TabFocusReason)
    }

    Connections {
        target: Chrome
        function onKeyboardNavigationModeChanged() {
            if (!Chrome.keyboardNavigationMode)
                volume.keyboardExpanded = false
        }
    }

    HoverHandler {
        id: extentHandler
    }

    ColumnLayout {
        anchors.horizontalCenter: parent.horizontalCenter
        anchors.verticalCenter: parent.verticalCenter
        spacing: Brume.gapSmall

        BrumeSlider {
            id: volumeSlider
            objectName: "volumeSlider"
            visible: volume.expanded
            orientation: Qt.Vertical
            implicitWidth: Brume.volumeWidth - 8
            implicitHeight: 140
            from: 0
            to: 100
            stepSize: 1
            enabled: volume.profile.canSetGain
            Accessible.name: "Playback volume"
            onMoved: if (Projection.bridge)
                Projection.bridge.setVolume(Math.round(value))
            Keys.onPressed: function(event) {
                Chrome.noteKeyboardActivity()
                if (event.key === Qt.Key_Backtab) {
                    if (Projection.bridge)
                        Projection.bridge.focusNextChrome("volume", true)
                    event.accepted = true
                }
            }
        }

        BrumeIconButton {
            id: muteButton
            objectName: "volumeMute"
            label: volume.profile.live.muted ? "Unmute" : "Mute"
            icon.source: Brume.icon(volume.profile.live.muted ? "volume-off" : "volume")
            enabled: volume.profile.canSetGain
            onClicked: if (Projection.bridge)
                Projection.bridge.setMuted(!volume.profile.live.muted)
            Keys.onPressed: function(event) {
                Chrome.noteKeyboardActivity()
                if (event.key === Qt.Key_Tab) {
                    if (Projection.bridge)
                        Projection.bridge.focusNextChrome("volume", false)
                    event.accepted = true
                }
            }
        }
    }

    Binding {
        target: volumeSlider
        property: "value"
        value: volume.profile.live.volumePercent
    }
}
