import QtQuick
import QtQuick.Controls

// Ordinary QtQuick Controls ScrollView with a single addition: when the
// window's active keyboard-focus item is a visible descendant of this scroll's
// content, the view is nudged — bounded to the real scroll range — just enough
// to show it. The inherited ScrollView API, defaults and content assignment are
// untouched; there is no new public property and no per-frame update loop.
ScrollView {
    id: control

    // Reveal the focused descendant without ever scrolling past the content.
    function revealActiveFocus() {
        const window = control.Window.window
        // ScrollView's content item is always its internal Flickable; the cast
        // only informs the type checker.
        const flick = control.contentItem as Flickable
        if (!window || !flick)
            return
        const focused = window.activeFocusItem
        if (!focused || focused === flick)
            return
        // Only descend within this scroll: unrelated windows or items leave the
        // parent walk before it reaches the flickable.
        let node = focused
        while (node && node !== flick)
            node = node.parent
        if (node !== flick)
            return
        if (!focused.visible || focused.width <= 0 || focused.height <= 0)
            return
        // Viewport-relative rectangle of the focused item (mapToItem folds in
        // the current content offset).
        const origin = focused.mapToItem(flick, 0, 0)
        const top = origin.y
        const bottom = origin.y + focused.height
        let target = flick.contentY
        if (top < 0)
            target += top
        else if (bottom > flick.height)
            target += bottom - flick.height
        const maximum = Math.max(0, flick.contentHeight - flick.height)
        flick.contentY = Math.max(0, Math.min(target, maximum))
    }

    // Coalesce focus and layout events into one reveal per event-loop turn;
    // restart() keeps a burst of events from scheduling more than one.
    Timer {
        id: revealCoalescer
        interval: 0
        repeat: false
        onTriggered: control.revealActiveFocus()
    }

    function scheduleReveal() {
        revealCoalescer.restart()
    }

    onContentHeightChanged: control.scheduleReveal()
    onHeightChanged: control.scheduleReveal()

    Connections {
        target: control.Window.window
        function onActiveFocusItemChanged() {
            control.scheduleReveal()
        }
    }
}
