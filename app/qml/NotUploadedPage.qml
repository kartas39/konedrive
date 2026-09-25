import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// What stays on this computer and is not uploaded, each with why
/// (Sync1's NotUploaded()): what is never uploaded, and the changes that
/// need the user before they can go up.
FormCard.FormCardPage {
    id: page

    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window
    property var shownBlocked: -1

    /// Loads the list when the page is shown, when the blocked count changes
    /// while it is, and when another account is chosen while it is.
    function load() {
        if (visible && sync) {
            shownBlocked = sync.blockedCount;
            sync.loadNotUploaded();
        }
    }

    objectName: "notUploadedPage"
    title: window ? window.accountTitle(i18nc("@title", "Not Uploaded")) : i18nc("@title", "Not Uploaded")

    onVisibleChanged: load()
    onSyncChanged: load()
    Connections {
        target: page.sync
        enabled: page.visible
        function onSyncChanged() {
            if (page.sync.blockedCount !== page.shownBlocked) {
                page.load();
            }
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing

        FormCard.FormPlaceholderMessageDelegate {
            visible: page.sync === null || page.sync.notUploaded.length === 0
            text: i18n("Nothing is kept back")
            explanation: i18n("Changes made here that cannot be uploaded — a name OneDrive refuses, a full OneDrive — and what is never uploaded, such as symbolic links, are listed here with the reason.")
            icon.name: "cloud-upload"
        }
        FormCard.FormSectionText {
            visible: page.sync !== null && page.sync.notUploaded.length > 0
            text: i18n("These stay on this computer and are not in OneDrive. Where the reason says what to do, the change goes up by itself once it is done.")
        }
        Repeater {
            model: page.sync ? page.sync.notUploaded : []
            delegate: FormCard.FormButtonDelegate {
                required property var modelData
                text: modelData.path
                description: modelData.why
                icon.name: "document-open-folder"
                onClicked: page.sync.showInFolder(modelData.path)
            }
        }
    }
}
