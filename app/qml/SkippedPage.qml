import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// What is in OneDrive but not in the folder, each with why.
FormCard.FormCardPage {
    id: page

    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window
    property var shownCount: -1

    /// Loads the list when the page is shown, when the count changes while
    /// it is, and when another account is chosen while it is.
    function load() {
        if (visible && sync) {
            shownCount = sync.skippedCount;
            sync.loadSkipped();
        }
    }

    objectName: "skippedPage"
    title: window ? window.accountTitle(i18nc("@title", "Not in the Folder")) : i18nc("@title", "Not in the Folder")

    onVisibleChanged: load()
    onSyncChanged: load()
    Connections {
        target: page.sync
        enabled: page.visible
        function onSyncChanged() {
            if (page.sync.skippedCount !== page.shownCount) {
                page.load();
            }
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing

        FormCard.FormPlaceholderMessageDelegate {
            visible: page.sync === null || page.sync.skipped.length === 0
            text: i18n("Everything is in the folder")
            explanation: i18n("Items OneDrive has that cannot be shown here — names too long for Linux, the Personal Vault, shared folders, OneNote notebooks — are listed here with the reason.")
            icon.name: "view-hidden"
        }
        FormCard.FormSectionText {
            visible: page.sync !== null && page.sync.skipped.length > 0
            text: i18n("These are in your OneDrive but not in the folder. What is inside a skipped folder is covered by the folder's own entry.")
        }
        Repeater {
            model: page.sync ? page.sync.skipped : []
            delegate: FormCard.FormTextDelegate {
                required property var modelData
                text: modelData.path
                description: modelData.why
                textItem.elide: Text.ElideMiddle
            }
        }
    }
}
