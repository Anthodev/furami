pragma Singleton

import QtQuick

// The single chrome visibility / popup / hold authority (FUR-029).
//
// QML alone owns shown / held / hidden, the open popup and the interaction
// holds. C++ owns exactly one GUI-thread precise idle timer exposed as
// bridge.setChromeIdleArmed(bool) and the bridge.chromeInactivity signal; this
// controller never starts a second timer and never arms on snapshot
// notifications, repaint or static hover.
QtObject {
    id: chrome

    // Assigned once by the root composition to the FuramiBridge instance.
    property var bridge: null

    // shown | held | hidden
    property string mode: "shown"
    // "" | source | size | rate | output | menu | save | reset
    property string popup: ""
    property int popupEpoch: 0
    property bool rootAvailable: false
    // Mirror of the native family-focus property. The bridge notifies only on a
    // real bool transition; chrome never maps while the Furami family is
    // inactive, and no deliberate raise/activation ever runs on background
    // events.
    readonly property bool familyActive: chrome.bridge !== null
        && chrome.bridge.chromeFamilyActive === true
    readonly property string mandatoryPopup: {
        if (!Projection.ready)
            return ""
        const settings = Projection.view.settings
        if (settings.closeDialog === "save" || settings.closeDialog === "warning")
            return "save"
        return settings.resetToken !== "0" ? "reset" : ""
    }
    readonly property bool mandatoryPopupOpen: chrome.mandatoryPopup !== ""
        || chrome.popup === "save" || chrome.popup === "reset"
    onMandatoryPopupChanged: chrome.syncSessionDialogs()
    onRootAvailableChanged: {
        if (chrome.rootAvailable) {
            chrome.syncSessionDialogs()
            chrome.show()
        } else {
            chrome.dismissForRootHide()
        }
    }
    // Slider pressed/dragging and similar real press/drag interactions only.
    property bool interactionHeld: false
    // Set by Tab/Backtab; cleared by any pointer activity.
    property bool keyboardNavigationMode: false
    // True while an active control of one chrome tool holds focus AND keyboard
    // navigation mode is on: only that combination inhibits auto-hide.
    property bool chromeHasFocus: false
    // objectName of the tool that currently owns chrome focus. Keeps the flag
    // correct when focus moves between the three tool windows.
    property string focusOwner: ""

    // Bumped only by real activity; the roots use it to reveal.
    property int activityCounter: 0

    readonly property bool chromeShowing: chrome.mode !== "hidden"
    readonly property bool popupOpen: chrome.popup !== ""

    // Text entry inside a chrome tool disables the global Space shortcut.
    property int textEntryCount: 0
    readonly property bool textEntryActive: chrome.textEntryCount > 0

    function setTextEntryActive(active) {
        chrome.textEntryCount = Math.max(0, chrome.textEntryCount + (active ? 1 : -1))
    }

    // Truthful mandatory-state handling: while the projection is not yet
    // available, or while a capture is opening and the chrome is the only
    // on-screen explanation, the chrome stays visible instead of fading.
    readonly property bool canAutoHide: Projection.ready
        && Projection.view.productPhase === "Active"
        && Projection.view.live.playbackStatus === "Live"
        && !Projection.view.paused && !Projection.view.opening
        && !Projection.view.closing
        && Projection.view.intentError === null
        && !Projection.view.output.needsAction

    // A product state that must reach the user on its own, without pointer
    // activity. Entering one reveals the chrome; snapshot updates that keep the
    // same state never restart the deadline.
    readonly property string mandatoryState: {
        const v = Projection.view
        if (!Projection.ready)
            return ""
        if (v.intentError)
            return "intent-error"
        if (v.productPhase === "Disconnected")
            return "disconnected"
        if (v.recovery.stage.length > 0)
            return "recovery"
        if (v.opening)
            return "opening"
        return ""
    }

    onMandatoryStateChanged: {
        if (chrome.mandatoryState.length === 0)
            return
        chrome.activityCounter += 1
        chrome.show()
    }

    // Any real hold stops the idle deadline entirely.
    readonly property bool holds: chrome.interactionHeld || chrome.popupOpen
        || (chrome.keyboardNavigationMode && chrome.chromeHasFocus)
        || !chrome.canAutoHide

    onHoldsChanged: chrome.reconcileVisibility()

    function arm(armed) {
        const b = chrome.bridge
        if (b && typeof b.setChromeIdleArmed === "function")
            b.setChromeIdleArmed(armed && chrome.familyActive)
    }

    // Real activity (pointer motion anywhere, native or overlay). Clears
    // keyboard navigation mode and restarts the complete deadline.
    function notePointerActivity() {
        chrome.keyboardNavigationMode = false
        // Actual focus is a window fact, not a navigation-mode flag.
        chrome.activityCounter += 1
        chrome.show()
    }

    // Tab / Backtab traversal.
    function noteKeyboardActivity() {
        chrome.keyboardNavigationMode = true
        chrome.activityCounter += 1
        chrome.show()
    }

    function show() {
        if (!chrome.rootAvailable) {
            chrome.mode = "hidden"
            chrome.arm(false)
            return
        }
        if (chrome.holds) {
            chrome.mode = "held"
            chrome.arm(false)
        } else {
            chrome.mode = "shown"
            chrome.arm(true)
        }
    }

    function reconcileVisibility() {
        if (!chrome.rootAvailable) {
            chrome.arm(false)
            return
        }
        if (chrome.holds) {
            if (chrome.mode !== "hidden")
                chrome.mode = "held"
            chrome.arm(false)
            return
        }
        if (chrome.mode === "hidden") {
            chrome.arm(false)
            return
        }
        if (chrome.mode === "held") {
            chrome.mode = "shown"
            chrome.arm(true)
            return
        }
        // Already shown and unheld: leave the running deadline untouched.
    }

    function setInteractionHeld(held) {
        if (chrome.interactionHeld === held)
            return
        chrome.interactionHeld = held
        if (held) {
            // Native pointer events already clear keyboard mode. A keyboard
            // button press must not be reclassified as pointer activity.
            chrome.activityCounter += 1
            chrome.show()
        } else {
            chrome.reconcileVisibility()
        }
    }

    function setChromeFocus(owner, hasFocus) {
        if (hasFocus) {
            chrome.focusOwner = owner
            chrome.chromeHasFocus = true
        } else if (chrome.focusOwner === owner) {
            chrome.focusOwner = ""
            chrome.chromeHasFocus = false
        }
    }

    function refreshCatalogs() {
        if (chrome.bridge && chrome.rootAvailable && !Projection.view.closing)
            chrome.bridge.refreshCatalogs()
    }


    function openPopup(name) {
        if (!chrome.rootAvailable || (chrome.mandatoryPopupOpen && name !== chrome.mandatoryPopup))
            return
        if (chrome.popup === name) {
            chrome.closePopup()
            return
        }
        chrome.popupEpoch += 1
        chrome.popup = name
        if (name === "source" || name === "size" || name === "rate")
            chrome.refreshCatalogs()
        chrome.show()
    }

    function syncSessionDialogs() {
        if (!chrome.rootAvailable || !Projection.ready)
            return
        const name = chrome.mandatoryPopup
        if (name !== "") {
            if (chrome.popup !== name) {
                chrome.popupEpoch += 1
                chrome.popup = name
            }
            chrome.show()
        } else if (chrome.popup === "save" || chrome.popup === "reset") {
            chrome.popup = ""
            chrome.reconcileVisibility()
        }
    }

    function closePopup() {
        if (!chrome.popupOpen || chrome.mandatoryPopupOpen)
            return false
        chrome.popup = ""
        chrome.reconcileVisibility()
        return true
    }

    // A delayed close belongs only to the epoch and tool that started it.
    // Return true only when restoring that instance's invoker is still valid.
    function popupClosed(region, epoch) {
        if (!chrome.rootAvailable || epoch !== chrome.popupEpoch)
            return false
        if (chrome.popupOpen) {
            if (chrome.popupRegion(chrome.popup) !== region || chrome.mandatoryPopupOpen)
                return false
            chrome.closePopup()
        }
        return !chrome.popupOpen
    }

    // Escape is consumed by mandatory surfaces but never invents a decision.
    function dismissPopup() {
        if (!chrome.popupOpen)
            return false
        if (!chrome.mandatoryPopupOpen)
            chrome.closePopup()
        return true
    }

    // Concealment is not a backend choice. Restoration re-reads pending tokens
    // even if the producer has not emitted a different JSON snapshot.
    function dismissForRootHide() {
        chrome.popupEpoch += 1
        chrome.popup = ""
        chrome.interactionHeld = false
        chrome.keyboardNavigationMode = false
        chrome.chromeHasFocus = false
        chrome.focusOwner = ""
        chrome.mode = "hidden"
        chrome.arm(false)
    }

    function hideIfIdle() {
        if (!chrome.rootAvailable || chrome.mode === "hidden")
            return
        if (chrome.holds) {
            chrome.reconcileVisibility()
            return
        }
        chrome.popup = ""
        chrome.mode = "hidden"
        chrome.arm(false)
    }

    // Which chrome tool owns a given popup surface.
    function popupRegion(name) {
        switch (name) {
        case "source":
        case "size":
        case "rate":
        case "output":
            return "top"
        case "menu":
        case "save":
        case "reset":
            return "bottom"
        default:
            return ""
        }
    }

    // QtObject has no default property, so the bridge connections are held as
    // an explicit property.
    property Connections bridgeConnections: Connections {
        target: chrome.bridge
        function onChromeInactivity() {
            chrome.hideIfIdle()
        }
        function onStateChanged() {
            chrome.reconcileVisibility()
        }
    }
}
