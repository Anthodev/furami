import QtQuick
import QtQuick.Controls
import dev.antho.furami

// Plain-text Brume label. Untrusted strings (device, output, diagnostics,
// loaded filter labels, recovery text) must go through this or any Text with
// textFormat: Text.PlainText so they can never be interpreted as markup.
Label {
    id: control

    // body | secondary | note | title | error
    property string variant: "body"
    property bool sensibleEllipsis: false

    font.family: Brume.fontFamily
    font.pixelSize: control.variant === "title" ? Brume.fontSizeTitle
        : control.variant === "note" ? Brume.fontSizeSmall
        : Brume.fontSize
    font.weight: control.variant === "title" ? Brume.fontWeightStrong : Brume.fontWeightNormal
    color: !control.enabled ? Brume.disabledText
        : control.variant === "secondary" || control.variant === "note" ? Brume.secondary
        : control.variant === "error" ? Brume.error
        : Brume.foreground
    textFormat: Text.PlainText
    wrapMode: Text.Wrap
    elide: control.sensibleEllipsis ? Text.ElideRight : Text.ElideNone
}
