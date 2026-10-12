import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// One selectable row of a catalog list.
//
// The selected state is derived from the projection, never stored locally: the
// row reports an explicit activation and the owning view maps the opaque row
// key back to the runtime action. Ineligible rows stay visible (muted) instead
// of disappearing. A per-row reason is never a visible paragraph in the Source
// and Output lists: those views keep it in the accessible description and
// surface the actionable error at catalog level, so `showReason` is off there.
AbstractButton {
    id: control

    property string caption: ""
    property string detail: ""
    property bool eligible: true
    property string reason: ""
    property bool showReason: true
    property bool selected: false

    // Control positions the contentItem at leftPadding, exactly where the
    // indicator is drawn, so the content must be indented past the indicator to
    // keep the two disjoint.
    readonly property int contentInset: control.indicator
        ? control.indicator.width + Brume.gap : 0

    signal activated()

    enabled: control.eligible
    focusPolicy: Qt.StrongFocus
    Keys.onPressed: function(event) { Chrome.noteKeyboardActivity() }
    implicitHeight: Math.max(Brume.controlHeight, contentItem.implicitHeight + 2 * Brume.gapSmall)
    implicitWidth: contentItem.implicitWidth + Brume.hitTarget
    leftPadding: Brume.gap
    rightPadding: Brume.gap
    topPadding: Brume.gapSmall
    bottomPadding: Brume.gapSmall
    Accessible.name: control.caption
        + (control.detail.length > 0 ? ", " + control.detail : "")
        + (control.reason.length > 0 ? ", " + control.reason : "")
    Accessible.checkable: true
    Accessible.checked: control.selected
    Accessible.onPressAction: control.activated()

    onPressedChanged: Chrome.setInteractionHeld(control.pressed)
    onClicked: control.activated()

    indicator: Rectangle {
        implicitWidth: 18
        implicitHeight: 18
        x: control.leftPadding
        y: control.topPadding + (control.availableHeight - height) / 2
        radius: width / 2
        color: "transparent"
        border.width: control.visualFocus ? 2 : 1
        border.color: !control.enabled ? Brume.disabledText
            : control.selected ? Brume.accent : Brume.divider

        Rectangle {
            anchors.centerIn: parent
            width: 8
            height: 8
            radius: 4
            color: Brume.accent
            visible: control.selected
        }
    }

    contentItem: ColumnLayout {
        id: content
        spacing: 2
        BrumeLabel {
            leftPadding: control.contentInset
            text: control.caption
            variant: control.enabled ? "body" : "secondary"
            Layout.fillWidth: true
        }
        BrumeLabel {
            leftPadding: control.contentInset
            text: control.detail
            variant: "secondary"
            visible: text.length > 0
            Layout.fillWidth: true
        }
        BrumeLabel {
            leftPadding: control.contentInset
            text: control.reason
            variant: "note"
            visible: control.showReason && text.length > 0
            Layout.fillWidth: true
        }
    }

    background: Rectangle {
        radius: Brume.radiusControl
        color: control.down ? Brume.controlPressed
            : control.hovered ? Brume.controlHover
            : control.selected ? Brume.control : "transparent"
        border.width: control.visualFocus ? 1 : 0
        border.color: Brume.accent
    }
}
