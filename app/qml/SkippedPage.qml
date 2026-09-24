import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// What is in OneDrive but not in the folder, each with why.
FormCard.FormCardPage {
    id: page

    property var shownCount: -1

    objectName: "skippedPage"
    title: i18nc("@title", "Not in the Folder")

    // Loaded when the page is shown, and again when the count changes while it is.
    onVisibleChanged: {
        if (visible) {
            shownCount = Sync.skippedCount;
            Sync.loadSkipped();
        }
    }
    Connections {
        target: Sync
        enabled: page.visible
        function onSyncChanged() {
            if (Sync.skippedCount !== page.shownCount) {
                page.shownCount = Sync.skippedCount;
                Sync.loadSkipped();
            }
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing

        FormCard.FormPlaceholderMessageDelegate {
            visible: Sync.skipped.length === 0
            text: i18n("Everything is in the folder")
            explanation: i18n("Items OneDrive has that cannot be shown here — names too long for Linux, the Personal Vault, shared folders, OneNote notebooks — are listed here with the reason.")
            icon.name: "view-hidden"
        }
        FormCard.FormSectionText {
            visible: Sync.skipped.length > 0
            text: i18n("These are in your OneDrive but not in the folder. What is inside a skipped folder is covered by the folder's own entry.")
        }
        Repeater {
            model: Sync.skipped
            delegate: FormCard.FormTextDelegate {
                required property var modelData
                text: modelData.path
                description: modelData.why
                textItem.elide: Text.ElideMiddle
            }
        }
    }
}
