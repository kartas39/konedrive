import QtQuick
import org.kde.kirigami as Kirigami
import org.konedrive.app

/// Disconnecting the account shown (Accounts.Remove): what it does to its folder.
UnsentChangesPrompt {
    id: dialog

    function confirm() {
        if (item) {
            Accounts.removeAccount(item.path);
        }
        close();
    }

    title: item ? i18nc("@title:window", "Disconnect %1?", item.account.label) : ""
    question: item && item.sync.rootPath.length > 0
              ? i18n("KOneDrive stops syncing %1 and signs out of the account. The files downloaded so far, and your own, stay there as plain files; the rest are only in OneDrive. Nothing in OneDrive is deleted.", item.sync.rootPath)
              : i18n("KOneDrive signs out of this account and forgets it on this device. Nothing in OneDrive is deleted.")
    customFooterActions: [
        Kirigami.Action {
            text: i18nc("@action:button", "Disconnect")
            icon.name: "list-remove-user"
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
