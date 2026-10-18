import QtQuick
import org.kde.kirigami as Kirigami
import org.konedrive.app

/// Using another folder for the account shown, whose folder works: this one is forgotten
/// (Folder.Unregister; what is in it stays, as plain files), and the account's Status offers
/// it a new one. A folder that stopped is given another in the step of choosing a folder.
UnsentChangesPrompt {
    id: dialog

    function confirm() {
        if (item) {
            item.sync.forget();
        }
        close();
    }

    title: i18nc("@title:window", "Use Another Folder?")
    question: item && item.sync.rootPath.length > 0
              ? i18n("KOneDrive stops syncing %1 and leaves the files downloaded so far, and your own, there as plain files. Then you choose an empty folder: your OneDrive shows up there, and files download again when you open them.", item.sync.rootPath)
              : ""
    customFooterActions: [
        Kirigami.Action {
            text: i18nc("@action:button", "Use Another Folder…")
            icon.name: "folder-open"
            enabled: dialog.item !== null
            onTriggered: dialog.confirm()
        },
        Kirigami.Action {
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: dialog.close()
        }
    ]
}
