pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Renderer settings: the nine typed fields of the existing RendererSettings
// DTO, driven by the runtime-projected catalog. No hardcoded scaler list.
Item {
    id: view

    implicitWidth: Brume.settingsWidth
    implicitHeight: layout.implicitHeight

    readonly property var profile: Projection.view
    readonly property var renderer: profile.renderer
    readonly property var settings: profile.selection.renderer

    // Fixed field identities keep local editor buffers alive when the producer
    // publishes fresh arrays. Availability, bounds and choices remain projected.
    readonly property var fieldKeys: [
        "scale", "cscale", "dscale", "dither", "scale_antiring",
        "deband", "deband_iterations", "deband_grain", "interpolation"
    ]
    function fieldRow(key) {
        const rows = view.renderer.fields
        for (let i = 0; i < rows.length; ++i) {
            if (rows[i].key === key)
                return rows[i]
        }
        return null
    }

    function dispatch(fieldKey, newValue) {
        if (!Projection.bridge)
            return
        const revision = profile.selectionRevision
        switch (fieldKey) {
        case "scale":
            Projection.bridge.setScale(revision, String(newValue))
            break
        case "cscale":
            Projection.bridge.setCscale(revision, String(newValue))
            break
        case "dscale":
            Projection.bridge.setDscale(revision, String(newValue))
            break
        case "scale_antiring":
            Projection.bridge.setScaleAntiring(revision, Number(newValue))
            break
        case "dither":
            Projection.bridge.setDither(revision, String(newValue))
            break
        case "deband":
            Projection.bridge.setDebandEnabled(revision, newValue === true)
            break
        case "deband_iterations":
            Projection.bridge.setDebandIterations(revision, Number(newValue))
            break
        case "deband_grain":
            Projection.bridge.setDebandGrain(revision, Number(newValue))
            break
        case "interpolation":
            Projection.bridge.setInterpolation(revision, newValue === true)
            break
        }
    }

    function currentValue(key) {
        const s = view.settings
        switch (key) {
        case "scale":
            return s.scale
        case "cscale":
            return s.cscale
        case "dscale":
            return s.dscale
        case "scale_antiring":
            return s.scale_antiring
        case "dither":
            return s.dither
        case "deband":
            return s.deband ? s.deband.enabled : false
        case "deband_iterations":
            return s.deband ? s.deband.iterations : 1
        case "deband_grain":
            return s.deband ? s.deband.grain : 32
        case "interpolation":
            return s.interpolation
        }
        return undefined
    }

    ColumnLayout {
        id: layout
        anchors.fill: parent
        spacing: Brume.gap

        BrumeLabel {
            objectName: "rendererError"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "error"
            text: view.renderer.error
        }

        BrumeLabel {
            objectName: "rendererNotice"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "note"
            text: view.renderer.notice
        }

        ColumnLayout {
            id: fields
            Layout.fillWidth: true
            enabled: view.renderer.available && !view.profile.closing
                && view.profile.canEditSelection && view.profile.selection.present
            spacing: Brume.gap

            Repeater {
                model: view.fieldKeys

                RendererField {
                    required property var modelData
                    Layout.fillWidth: true
                    readonly property var projectedRow: view.fieldRow(modelData)
                    visible: projectedRow !== null && view.profile.selection.present
                    row: projectedRow
                    value: view.currentValue(modelData)
                    onSetRequested: function(fieldKey, newValue) {
                        view.dispatch(fieldKey, newValue)
                    }
                }
            }
        }

        BrumeLabel {
            Layout.fillWidth: true
            visible: view.renderer.fields.length === 0
            variant: "note"
            text: view.renderer.available
                ? "No rendering field reported."
                : "Rendering catalog unavailable."
        }
    }
}
