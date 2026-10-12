pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import dev.antho.furami

// Truthful session context: real error/diagnostic text, recovery candidates and
// the settings refusal notice. Every dynamic string is untrusted and rendered
// as plain text. This surface is operational truth only (validation, failures,
// recovery, command errors, native failures); the stream *statistics* live in
// their own bottom overlay label and are governed by the stats preference.
Item {
    id: context

    readonly property var profile: Projection.view
    readonly property string diagnostic: Projection.bridge
        ? String(Projection.bridge.contextDiagnostic || "") : ""
    readonly property bool hasError: profile.intentError !== null
        || diagnostic.length > 0
        || Projection.failureText.length > 0
    readonly property bool hasRecovery: profile.recovery.candidates.length > 0
        || profile.recovery.stage.length > 0
    readonly property bool hasNotice: profile.settings.refused || profile.output.needsAction
    readonly property bool visibleContent: context.hasError || context.hasRecovery || context.hasNotice

    implicitWidth: content.implicitWidth
    implicitHeight: context.visibleContent ? content.implicitHeight : 0

    function chooseRecovery(key) {
        if (Projection.bridge)
            Projection.bridge.chooseRecovery(context.profile.generation, key)
    }

    ColumnLayout {
        id: content
        width: parent.width
        spacing: Brume.gapSmall

        BrumeLabel {
            objectName: "contextIntentError"
            Layout.fillWidth: true
            visible: context.profile.intentError !== null
            variant: "error"
            text: context.profile.intentError
                ? (String(context.profile.intentError.message || "").length > 0
                    ? String(context.profile.intentError.message)
                    : String(context.profile.intentError.code || ""))
                : ""
        }

        BrumeLabel {
            objectName: "contextProjectionFailure"
            Layout.fillWidth: true
            visible: Projection.failureText.length > 0
            variant: "error"
            text: Projection.failureText
        }

        BrumeLabel {
            objectName: "contextDiagnostic"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "note"
            text: context.diagnostic
        }

        BrumeLabel {
            objectName: "contextSettingsRefused"
            Layout.fillWidth: true
            visible: context.profile.settings.refused
            variant: "error"
            text: "Fichier de réglages refusé : les changements restent valables pour cette session."
        }

        BrumeLabel {
            objectName: "contextOutputAction"
            Layout.fillWidth: true
            visible: context.profile.output.needsAction
            variant: "error"
            text: "Sortie audio manuelle introuvable : reselectionnez une sortie dans le menu (la capture ne redémarre pas)."
        }

        BrumeLabel {
            objectName: "contextRecoveryStage"
            Layout.fillWidth: true
            visible: text.length > 0
            variant: "secondary"
            text: context.profile.recovery.stage
                + (context.profile.recovery.evidence.length > 0
                    ? " — " + context.profile.recovery.evidence : "")
        }

        Repeater {
            model: context.profile.recovery.candidates

            RowLayout {
                id: candidateRow
                required property var modelData
                Layout.fillWidth: true
                spacing: Brume.gapSmall

                BrumeLabel {
                    Layout.fillWidth: true
                    text: String(candidateRow.modelData.label || "")
                        + (String(candidateRow.modelData.detail || "").length > 0
                            ? " — " + String(candidateRow.modelData.detail) : "")
                    variant: "note"
                }
                BrumeButton {
                    objectName: "recoveryUse"
                    label: "Reconnecter"
                    enabled: candidateRow.modelData.eligible !== false
                    Accessible.name: "Reconnecter cette source"
                    onClicked: context.chooseRecovery(String(candidateRow.modelData.key))
                }
            }
        }
    }
}
