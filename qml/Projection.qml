pragma Singleton

import QtQuick

// Read-only projection of the Rust `ui_snapshot` value contract (FUR-029).
//
// QML parses the snapshot once per notification and never converts an opaque
// decimal token into a number. Every u64 identity (state token, generation,
// selection revision, catalog/output revisions, reset token, recovery key)
// stays a JavaScript string end to end. `selection` is the latest requested
// configuration, never a verified applied result.
QtObject {
    id: projection

    // Assigned once by the root composition to the FuramiBridge instance.
    property var bridge: null

    // Parsed projection, or null while no usable snapshot has arrived.
    property var snapshot: null

    // Invalid producer values are rejected before any control can consume them.
    property string failure: ""
    property string failureField: ""

    readonly property bool ready: projection.snapshot !== null

    readonly property string raw: {
        const b = projection.bridge
        return (b && typeof b.uiSnapshot === "string") ? b.uiSnapshot : ""
    }

    onRawChanged: projection.reload()
    onBridgeChanged: projection.reload()
    Component.onCompleted: projection.reload()

    function reload() {
        projection.failureField = ""
        const text = projection.raw
        if (text.length === 0) {
            projection.snapshot = null
            projection.failure = ""
            return
        }
        let parsed = null
        try {
            parsed = JSON.parse(text)
        } catch (error) {
            projection.snapshot = null
            projection.failure = "unparseable"
            return
        }
        if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
            projection.snapshot = null
            projection.failure = "not-object"
            return
        }
        // The producer writes schema 2 (src/ui/projection.rs Snapshot::schema).
        // An unknown, missing or older version is an honest failure, never
        // normalised into an apparently valid empty state.
        if (typeof parsed.schema !== "number") {
            projection.snapshot = null
            projection.failure = "missing-schema"
            return
        }
        if (parsed.schema !== 2) {
            projection.snapshot = null
            projection.failure = "unsupported-schema"
            return
        }
        const invalidField = projection.snapshotError(parsed)
        if (invalidField !== "") {
            projection.snapshot = null
            projection.failure = "invalid-contract"
            projection.failureField = invalidField
            return
        }
        projection.snapshot = parsed
        projection.failure = ""
    }

    // Display text for the honest failure above. Never used to decide action
    // legality.
    readonly property string failureText: {
        switch (projection.failure) {
        case "unparseable":
            return "Unreadable projection: no state produced."
        case "not-object":
            return "Invalid projection: no state produced."
        case "missing-schema":
            return "Projection without schema: no state produced."
        case "unsupported-schema":
            return "Projection version unsupported by this interface."
        case "invalid-contract":
            return "Invalid projection (" + projection.failureField
                + "): controls disabled."
        }
        return ""
    }

    // Only schema-2 values are accepted. These checks describe this fixed
    // producer contract; they never repair data or decide media availability.
    function strings(value, keys) {
        return projection.record(value) !== null
            && keys.every(function(key) { return typeof value[key] === "string" })
    }
    function flags(value, keys) {
        return projection.record(value) !== null
            && keys.every(function(key) { return typeof value[key] === "boolean" })
    }
    function numbers(value, keys) {
        return projection.record(value) !== null
            && keys.every(function(key) {
                return typeof value[key] === "number" && isFinite(value[key])
            })
    }
    function u64(value) {
        // Lexical comparison keeps u64-max exact, including values above 2^53.
        if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value))
            return false
        const maximum = "18446744073709551615"
        return value.length < maximum.length
            || (value.length === maximum.length && value <= maximum)
    }
    function u32(value, minimum) {
        return typeof value === "number" && isFinite(value)
            && Math.floor(value) === value && value >= minimum && value <= 4294967295
    }
    function sizeValid(value) {
        return projection.record(value) !== null
            && projection.u32(value.width, 1) && projection.u32(value.height, 1)
    }
    function rationalValid(value, minimum) {
        return projection.record(value) !== null
            && projection.u32(value.numerator, minimum)
            && projection.u32(value.denominator, 1)
    }
    function rendererSettingsValid(value) {
        return projection.strings(value, ["scale", "cscale", "dscale", "dither"])
            && projection.numbers(value, ["scale_antiring"])
            && projection.flags(value, ["interpolation"])
            && projection.flags(value.deband, ["enabled"])
            && projection.numbers(value.deband, ["iterations", "grain"])
            && Math.floor(value.deband.iterations) === value.deband.iterations
    }
    function selectionValid(value) {
        if (value === null)
            return true
        return projection.strings(value, ["sourceKey", "sourceLabel", "formatPolicy",
                "manualFormatKey", "requestedFormatLabel", "resolvedFormatLabel",
                "audioInputKey", "audioInputLabel"])
            && projection.flags(value, ["sourceAvailable", "audioEnabled"])
            && (value.size === null || projection.sizeValid(value.size))
            && (value.rate === null || (projection.rationalValid(value.rate, 1)
                && projection.strings(value.rate, ["text"])))
            && (value.formatPolicy === "auto" || value.formatPolicy === "manual")
            && projection.rendererSettingsValid(value.renderer)
    }

    // Structured stream statistics, or null while no engine/settings exist.
    // Every dimension is validated against the same Size/Rate contracts used by
    // the selection projection; the overlay never parses a diagnostic string.
    function streamStatsValid(value) {
        if (value === null)
            return true
        return projection.strings(value, ["identityText", "inputPolicy",
                "resolvedInput", "audioInputLabel"])
            && projection.flags(value, ["audioEnabled"])
            && projection.sizeValid(value.size)
            && projection.rationalValid(value.rate, 1)
            && projection.strings(value.rate, ["text"])
    }
    // Diagnostics health (FUR-023), an additive schema-2 block: the three
    // counters reuse the same u64 decimal-string contract as every identity,
    // while `status` and `folderState` are closed producer vocabularies.
    function diagnosticsValid(value) {
        if (!projection.strings(value, ["status", "path", "message", "folderState"])
                || !projection.flags(value, ["hasProblem"])
                || !projection.u64(value.droppedEvents)
                || !projection.u64(value.oversizedRecords)
                || !projection.u64(value.oversizedReports))
            return false
        if (["Starting", "Healthy", "Degraded", "Unavailable", "Stopped", "Disabled"]
                .indexOf(value.status) < 0)
            return false
        return ["Idle", "Preparing", "OpenRequested", "Failed"]
            .indexOf(value.folderState) >= 0
    }
    function choiceValid(value) {
        return projection.strings(value, ["key", "label", "reason"])
            && projection.flags(value, ["eligible"])
    }
    function catalogError(c) {
        if (!projection.strings(c, ["revision", "error", "selectedSourceKey",
                "selectedAudioInputKey", "capabilityStatus", "capabilityReason"])
                || !projection.flags(c, ["busy"]) || !projection.u64(c.revision))
            return "catalog"
        if (["ready", "querying", "unknown", "unavailable"].indexOf(c.capabilityStatus) < 0)
            return "catalog.capabilityStatus"
        if (!Array.isArray(c.sourceRows) || !c.sourceRows.every(function(row) {
            return projection.choiceValid(row)
                && projection.strings(row, ["detail", "vendorId", "productId", "topology", "nodeLabel"])
                && (row.serial === null || typeof row.serial === "string")
        }))
            return "catalog.sourceRows"
        if (!Array.isArray(c.formatRows) || !c.formatRows.every(function(row) {
            return projection.choiceValid(row) && projection.strings(row, ["fourcc"])
        }))
            return "catalog.formatRows"
        if (!Array.isArray(c.sizeRows) || !c.sizeRows.every(function(row) {
            return projection.choiceValid(row) && projection.sizeValid(row)
        }))
            return "catalog.sizeRows"
        if (!Array.isArray(c.rateRows) || !c.rateRows.every(function(row) {
            return projection.choiceValid(row) && projection.rationalValid(row, 1)
                && projection.strings(row, ["exact"])
        }))
            return "catalog.rateRows"
        if (!Array.isArray(c.audioInputRows) || !c.audioInputRows.every(projection.choiceValid))
            return "catalog.audioInputRows"
        if (!Array.isArray(c.sizeRanges) || !c.sizeRanges.every(function(range) {
            return projection.strings(range, ["formatKey", "kind"])
                && (range.kind === "continuous" || range.kind === "stepwise")
                && projection.sizeValid(range.min) && projection.sizeValid(range.max)
                && (range.stepWidth === null || projection.u32(range.stepWidth, 0))
                && (range.stepHeight === null || projection.u32(range.stepHeight, 0))
        }))
            return "catalog.sizeRanges"
        if (!Array.isArray(c.intervalRanges) || !c.intervalRanges.every(function(range) {
            return projection.strings(range, ["formatKey", "kind"])
                && (range.kind === "continuous" || range.kind === "stepwise")
                && projection.rationalValid(range.min, 1) && projection.rationalValid(range.max, 1)
                && (range.step === null || projection.rationalValid(range.step, 0))
        }))
            return "catalog.intervalRanges"
        return ""
    }
    function rendererFieldValid(row) {
        if (!projection.strings(row, ["key", "label", "type", "notice"])
                || !Array.isArray(row.choices) || !row.choices.every(projection.choiceValid))
            return false
        const types = {
            scale: "choice", cscale: "choice", dscale: "choice", dither: "choice",
            scale_antiring: "number", deband: "boolean", deband_iterations: "integer",
            deband_grain: "number", interpolation: "boolean"
        }
        if (types[row.key] !== row.type)
            return false
        return ["min", "max", "step"].every(function(key) {
            return row[key] === null || (typeof row[key] === "number" && isFinite(row[key]))
        }) && ((row.type !== "number" && row.type !== "integer")
            || (typeof row.min === "number" && typeof row.max === "number"))
    }
    function snapshotError(s) {
        if (s.session_kind !== "capture" && s.session_kind !== "qualification-video")
            return "session_kind"
        if (!projection.strings(s, ["stateToken", "generation", "productPhase", "gatePhase",
                "selectionRevision"]) || s.stateToken.indexOf("state:") !== 0
                || !projection.u64(s.stateToken.slice(6)) || !projection.u64(s.generation)
                || !projection.u64(s.selectionRevision))
            return "stateToken / generation / selectionRevision"
        if (!projection.flags(s, ["paused", "opening", "closing", "canEditSelection",
                "canRestart", "canReconnect", "canTogglePause", "canSetGain"]))
            return "state / permissions"
        if (!projection.selectionValid(s.selection))
            return "selection"
        if (!projection.selectionValid(s.applied))
            return "applied"
        if (!projection.selectionValid(s.lastValid))
            return "lastValid"
        if (!projection.streamStatsValid(s.streamStats))
            return "streamStats"
        if (!projection.strings(s.live, ["sourceLabel", "sizeLabel", "rateLabel", "formatLabel",
                "audioStatus", "audioDiagnostic", "playbackStatus"])
                || !projection.flags(s.live, ["muted"])
                || !projection.u32(s.live.volumePercent, 0) || s.live.volumePercent > 100)
            return "live"
        const catalogFailure = projection.catalogError(s.catalog)
        if (catalogFailure !== "")
            return catalogFailure
        const r = s.renderer
        if (!projection.strings(r, ["error", "notice"]) || !projection.flags(r, ["available"])
                || !Array.isArray(r.fields) || !r.fields.every(projection.rendererFieldValid))
            return "renderer"
        if (r.available && r.fields.length !== 9)
            return "renderer"
        if (r.fields.some(function(row, index) {
            return r.fields.some(function(other, otherIndex) {
                return otherIndex < index && other.key === row.key
            })
        }))
            return "renderer.fields (duplicate identity)"
        const o = s.output
        if (!projection.strings(o, ["revision", "selectedKey", "selectedLabel", "effectiveLabel", "status"])
                || !projection.u64(o.revision) || !projection.flags(o, ["needsAction"])
                || !Array.isArray(o.rows) || !o.rows.every(function(row) {
                    return projection.choiceValid(row) && projection.strings(row, ["name"])
                }))
            return "output"
        const recovery = s.recovery
        if (!projection.strings(recovery, ["evidence", "stage"])
                || !Array.isArray(recovery.candidates) || !recovery.candidates.every(function(row) {
                    if (!projection.strings(row, ["key", "label", "detail"])
                            || !projection.flags(row, ["eligible"]))
                        return false
                    const tokens = row.key.split(":")
                    return tokens.length === 3 && tokens.every(projection.u64)
                }))
            return "recovery"
        const settings = s.settings
        if (!projection.strings(settings, ["status", "attention", "path", "startupReason",
                "closeDialog", "resetToken"]) || !projection.flags(settings, ["refused"])
                || !projection.u64(settings.resetToken)
                || ["", "save", "warning"].indexOf(settings.closeDialog) < 0)
            return "settings"
        if (!projection.diagnosticsValid(s.diagnostics))
            return "diagnostics"
        if (s.intentError !== null) {
            if (!projection.strings(s.intentError, ["code", "action", "field", "message"])
                    || ["stale_selection", "stale_catalog", "invalid_value",
                        "unavailable", "busy", "closing", "capture_error", "audio_error",
                        "renderer_error"].indexOf(s.intentError.code) < 0)
                return "intentError"
        }
        return ""
    }


    // ── normalizers ────────────────────────────────────────────────────────
    function text(value) {
        return typeof value === "string" ? value : ""
    }
    function number(value, fallback) {
        return (typeof value === "number" && isFinite(value)) ? value : fallback
    }
    function flag(value) {
        return value === true
    }
    function list(value) {
        return Array.isArray(value) ? value : []
    }
    function record(value) {
        return (value !== null && typeof value === "object" && !Array.isArray(value)) ? value : null
    }

    readonly property var emptySelection: ({
        present: false,
        sourceKey: "",
        sourceLabel: "",
        sourceAvailable: false,
        size: null,
        rate: null,
        formatPolicy: "auto",
        manualFormatKey: "",
        requestedFormatLabel: "",
        resolvedFormatLabel: "",
        audioEnabled: false,
        audioInputKey: "",
        audioInputLabel: "",
        renderer: {}
    })

    function selectionView(value) {
        const s = projection.record(value)
        if (!s) return projection.emptySelection
        return {
            present: true,
            sourceKey: projection.text(s.sourceKey),
            sourceLabel: projection.text(s.sourceLabel),
            sourceAvailable: projection.flag(s.sourceAvailable),
            size: projection.record(s.size),
            rate: projection.record(s.rate),
            formatPolicy: projection.text(s.formatPolicy),
            manualFormatKey: projection.text(s.manualFormatKey),
            requestedFormatLabel: projection.text(s.requestedFormatLabel),
            resolvedFormatLabel: projection.text(s.resolvedFormatLabel),
            audioEnabled: projection.flag(s.audioEnabled),
            audioInputKey: projection.text(s.audioInputKey),
            audioInputLabel: projection.text(s.audioInputLabel),
            renderer: projection.record(s.renderer) || {}
        }
    }

    // Structured stream statistics for the bottom overlay. `null` while the
    // producer has no engine/settings; the empty view carries `null` too.
    function streamStatsView(value) {
        const s = projection.record(value)
        if (!s) return null
        return {
            identityText: projection.text(s.identityText),
            inputPolicy: projection.text(s.inputPolicy),
            size: projection.record(s.size),
            rate: projection.record(s.rate),
            resolvedInput: projection.text(s.resolvedInput),
            audioEnabled: projection.flag(s.audioEnabled),
            audioInputLabel: projection.text(s.audioInputLabel)
        }
    }

    // Size/rate helpers usable directly from bindings without null checks.
    function sizeText(selection) {
        const s = selection && selection.present ? selection.size : null
        return s ? (s.width + " × " + s.height) : ""
    }
    function rateText(selection) {
        const r = selection && selection.present ? selection.rate : null
        return r ? projection.text(r.text) : ""
    }

    // Human framerate label for an exact `N` or `N/D` rational. An integral
    // rate stays a whole number ("60 FPS"), detected by exact divisibility; a
    // genuine fraction is shown as a rounded two-decimal FPS value (decimal
    // dot, redundant zeros removed), so `60000/1001` reads `59.94 FPS`. The
    // display is an approximation only: the exact rational stays available
    // through rateText and the exact editor. Any non-rational text ("" or the
    // producer's own label) is returned verbatim.
    function rateFps(exact) {
        const text = typeof exact === "string" ? exact : ""
        const parts = text.split("/")
        if (parts.length > 2 || !/^[0-9]+$/.test(parts[0]))
            return text
        const denominatorText = parts.length === 2 ? parts[1] : "1"
        if (!/^[0-9]+$/.test(denominatorText))
            return text
        const numerator = Number(parts[0])
        const denominator = Number(denominatorText)
        if (denominator === 0
                || !Number.isSafeInteger(numerator) || !Number.isSafeInteger(denominator))
            return text
        if (numerator % denominator === 0)
            return String(numerator / denominator) + " FPS"
        const rounded = (numerator / denominator).toFixed(2)
            .replace(/0+$/, "").replace(/\.$/, "")
        return rounded + " FPS"
    }

    // Display-only label for the producer's product phase vocabulary
    // (src/ui/runtime.rs phase_name). Any unexpected token is shown verbatim:
    // this mapping never decides action legality and never invents a state.
    function productPhaseText(phase) {
        switch (phase) {
        case "Idle":
            return "No capture"
        case "Stopped":
            return "Capture stopped"
        case "Active":
            return "Playing"
        case "Paused":
            return "Playback paused"
        case "PausePending":
            return "Pausing…"
        case "Validating":
            return "Validating…"
        case "ClosingOld":
            return "Closing media…"
        case "ApplyingTreatments":
            return "Applying treatments…"
        case "RestoringTreatments":
            return "Restoring treatments…"
        case "OpeningCandidate":
            return "Opening source…"
        case "CleaningFailedCandidate":
            return "Cleaning up after failure…"
        case "ValidatingPrior":
            return "Validating previous session…"
        case "OpeningRestore":
            return "Restoring media…"
        case "CleaningFailedRestore":
            return "Cleaning up after failure…"
        case "ValidatingResume":
            return "Validating resume…"
        case "ClosingResume":
            return "Closing for resume…"
        case "OpeningResume":
            return "Resuming playback…"
        case "CleaningFailedResume":
            return "Cleaning up after failure…"
        case "ErrorWithActiveRestored":
            return "Error — capture restored"
        case "ErrorWithoutActive":
            return "Error — no active capture"
        case "Disconnected":
            return "Capture disconnected"
        case "Recovering":
            return "Recovering…"
        case "SelectionRequired":
            return "Source required"
        case "Stopping":
            return "Stopping…"
        case "ShutdownReady":
            return "Shutdown complete"
        }
        return phase
    }

    function liveView(value) {
        const l = projection.record(value)
        if (!l) return projection.emptyView.live
        return {
            sourceLabel: projection.text(l.sourceLabel),
            sizeLabel: projection.text(l.sizeLabel),
            rateLabel: projection.text(l.rateLabel),
            formatLabel: projection.text(l.formatLabel),
            audioStatus: projection.text(l.audioStatus),
            audioDiagnostic: projection.text(l.audioDiagnostic),
            playbackStatus: projection.text(l.playbackStatus),
            volumePercent: projection.number(l.volumePercent, 0),
            muted: projection.flag(l.muted)
        }
    }

    function catalogView(value) {
        const c = projection.record(value)
        if (!c) return projection.emptyView.catalog
        return {
            revision: projection.text(c.revision),
            busy: projection.flag(c.busy),
            error: projection.text(c.error),
            sourceRows: projection.list(c.sourceRows),
            selectedSourceKey: projection.text(c.selectedSourceKey),
            formatRows: projection.list(c.formatRows),
            sizeRows: projection.list(c.sizeRows),
            rateRows: projection.list(c.rateRows),
            sizeRanges: projection.list(c.sizeRanges),
            intervalRanges: projection.list(c.intervalRanges),
            audioInputRows: projection.list(c.audioInputRows),
            selectedAudioInputKey: projection.text(c.selectedAudioInputKey),
            capabilityStatus: projection.text(c.capabilityStatus),
            capabilityReason: projection.text(c.capabilityReason)
        }
    }


    function rendererView(value) {
        const r = projection.record(value)
        if (!r) return projection.emptyView.renderer
        return {
            available: projection.flag(r.available),
            error: projection.text(r.error),
            fields: projection.list(r.fields),
            notice: projection.text(r.notice)
        }
    }

    function outputView(value) {
        const o = projection.record(value)
        if (!o) return projection.emptyView.output
        return {
            revision: projection.text(o.revision),
            rows: projection.list(o.rows),
            selectedKey: projection.text(o.selectedKey),
            selectedLabel: projection.text(o.selectedLabel),
            effectiveLabel: projection.text(o.effectiveLabel),
            status: projection.text(o.status),
            needsAction: projection.flag(o.needsAction)
        }
    }

    function recoveryView(value) {
        const r = projection.record(value)
        if (!r) return projection.emptyView.recovery
        return {
            evidence: projection.text(r.evidence),
            stage: projection.text(r.stage),
            candidates: projection.list(r.candidates)
        }
    }

    function settingsView(value) {
        const s = projection.record(value)
        if (!s) return projection.emptyView.settings
        return {
            status: projection.text(s.status),
            attention: projection.text(s.attention),
            path: projection.text(s.path),
            refused: projection.flag(s.refused),
            startupReason: projection.text(s.startupReason),
            closeDialog: projection.text(s.closeDialog),
            resetToken: projection.text(s.resetToken)
        }
    }

    // Diagnostics health for the Options folder action and the always-visible
    // plain-text error. `path`/`message` are untrusted text rendered only
    // through BrumeLabel; the counters stay decimal strings end to end.
    function diagnosticsView(value) {
        const d = projection.record(value)
        if (!d) return projection.emptyView.diagnostics
        return {
            status: projection.text(d.status),
            path: projection.text(d.path),
            message: projection.text(d.message),
            hasProblem: projection.flag(d.hasProblem),
            droppedEvents: projection.text(d.droppedEvents),
            oversizedRecords: projection.text(d.oversizedRecords),
            oversizedReports: projection.text(d.oversizedReports),
            folderState: projection.text(d.folderState)
        }
    }

    readonly property var emptyView: ({
        schema: 0,
        session_kind: "capture",
        stateToken: "",
        generation: "",
        productPhase: "",
        gatePhase: "",
        paused: false,
        opening: false,
        closing: false,
        canEditSelection: false,
        canRestart: false,
        canReconnect: false,
        canTogglePause: false,
        canSetGain: false,
        selectionRevision: "",
        selection: projection.emptySelection,
        applied: projection.emptySelection,
        lastValid: projection.emptySelection,
        streamStats: null,
        live: {
            sourceLabel: "", sizeLabel: "", rateLabel: "", formatLabel: "",
            audioStatus: "", audioDiagnostic: "", playbackStatus: "",
            volumePercent: 0, muted: false
        },
        catalog: {
            revision: "", busy: false, error: "", sourceRows: [], selectedSourceKey: "",
            formatRows: [], sizeRows: [], rateRows: [], sizeRanges: [], intervalRanges: [],
            audioInputRows: [], selectedAudioInputKey: "",
            capabilityStatus: "unknown", capabilityReason: ""
        },
        renderer: { available: false, error: "", fields: [], notice: "" },
        output: {
            revision: "", rows: [], selectedKey: "", selectedLabel: "",
            effectiveLabel: "", status: "", needsAction: false
        },
        recovery: { evidence: "", stage: "", candidates: [] },
        settings: {
            status: "", attention: "", path: "", refused: false, startupReason: "",
            closeDialog: "", resetToken: ""
        },
        diagnostics: {
            status: "Starting", path: "", message: "", hasProblem: false,
            droppedEvents: "0", oversizedRecords: "0", oversizedReports: "0",
            folderState: "Idle"
        },
        intentError: null
    })

    readonly property var view: {
        const s = projection.snapshot
        if (!s) return projection.emptyView
        return {
            schema: projection.number(s.schema, 0),
            session_kind: projection.text(s.session_kind),
            stateToken: projection.text(s.stateToken),
            generation: projection.text(s.generation),
            productPhase: projection.text(s.productPhase),
            gatePhase: projection.text(s.gatePhase),
            paused: projection.flag(s.paused),
            opening: projection.flag(s.opening),
            closing: projection.flag(s.closing),
            canEditSelection: projection.flag(s.canEditSelection),
            canRestart: projection.flag(s.canRestart),
            canReconnect: projection.flag(s.canReconnect),
            canTogglePause: projection.flag(s.canTogglePause),
            canSetGain: projection.flag(s.canSetGain),
            selectionRevision: projection.text(s.selectionRevision),
            selection: projection.selectionView(s.selection),
            applied: projection.selectionView(s.applied),
            lastValid: projection.selectionView(s.lastValid),
            streamStats: projection.streamStatsView(s.streamStats),
            live: projection.liveView(s.live),
            catalog: projection.catalogView(s.catalog),
            renderer: projection.rendererView(s.renderer),
            output: projection.outputView(s.output),
            recovery: projection.recoveryView(s.recovery),
            settings: projection.settingsView(s.settings),
            diagnostics: projection.diagnosticsView(s.diagnostics),
            intentError: projection.record(s.intentError)
        }
    }
}
