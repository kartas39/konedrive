import QtQuick
import org.kde.kirigami as Kirigami
import org.konedrive.app

/// Using another folder for the account shown, whose folder works: this one is forgotten
/// (Folder.Unregister; what is in it stays, as plain files), and the window shows the
/// account's Status, which has Choose Folder… for a new one. A folder that stopped is given another by Use Another
/// Folder… on Status.
UnsentChangesPrompt {
    id: dialog

    /// The folder is being forgotten: Status, where the next one is chosen, can be shown.
    signal confirmed()

    function confirm() {
        if (item) {
            item.sync.forget();
            confirmed();
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
