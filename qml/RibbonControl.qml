import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// One ribbon control of the top chrome: leading icon, caption, live value and
// a chevron. Every selection is applied immediately, so there is no pending
// indicator.
Button {
    id: control

    property string iconName: ""
    property string caption: ""
    property string value: ""
    property bool showChevron: true

    focusPolicy: Qt.StrongFocus
    Keys.onPressed: function(event) { Chrome.noteKeyboardActivity() }
    implicitHeight: Brume.ribbonHeight
    implicitWidth: Math.max(132, contentItem.implicitWidth + leftPadding + rightPadding)
    leftPadding: Brume.edge
    rightPadding: Brume.gap
    Accessible.name: control.caption + ", " + control.value

    onPressedChanged: Chrome.setInteractionHeld(control.pressed)

    contentItem: RowLayout {
        spacing: Brume.gap

        BrumeIcon {
            iconSize: Brume.ribbonIconSize
            source: Brume.icon(control.iconName)
            tint: control.enabled ? Brume.secondary : Brume.disabledText
            Layout.alignment: Qt.AlignVCenter
        }

        ColumnLayout {
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignVCenter
            spacing: 0

            BrumeLabel {
                text: control.caption
                variant: "note"
                wrapMode: Text.NoWrap
                elide: Text.ElideRight
                Layout.fillWidth: true
            }
            BrumeLabel {
                text: control.value
                variant: control.enabled ? "body" : "secondary"
                wrapMode: Text.NoWrap
                elide: Text.ElideRight
                Layout.fillWidth: true
            }
        }

        BrumeIcon {
            iconSize: 14
            source: Brume.icon("chevron-down")
            tint: control.enabled ? Brume.secondary : Brume.disabledText
            visible: control.showChevron
            Layout.alignment: Qt.AlignVCenter
        }
    }

    background: Rectangle {
        color: control.down ? Brume.controlPressed
            : control.hovered ? Brume.controlHover
            : "transparent"
        radius: Brume.radiusControl
        border.width: control.visualFocus ? 2 : 0
        border.color: Brume.accent
    }
}
