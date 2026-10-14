import QtQuick
import dev.antho.furami

// Passive pointer-motion reporter for one chrome tool.
//
// Only actual pointer movement counts as activity. Entering or leaving the
// item (which a mapped-but-faded tool can trigger on its own) is deliberately
// ignored, so a stationary cursor over a control still fades after the idle
// deadline.
Item {
    id: activity

    anchors.fill: parent

    HoverHandler {
        id: handler
        property point lastPoint: Qt.point(-1, -1)

        onHoveredChanged: {
            if (!handler.hovered)
                handler.lastPoint = Qt.point(-1, -1)
        }

        onPointChanged: {
            if (!handler.hovered) {
                handler.lastPoint = Qt.point(-1, -1)
                return
            }
            const p = handler.point.position
            if (handler.lastPoint.x < 0) {
                // First sample after entering: not movement.
                handler.lastPoint = p
                return
            }
            if (p.x === handler.lastPoint.x && p.y === handler.lastPoint.y)
                return
            handler.lastPoint = p
            Chrome.notePointerActivity()
        }
    }
}
