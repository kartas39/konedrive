import QtQuick
import org.kde.kirigami as Kirigami
import org.konedrive.app

/// Disconnecting the account shown (Accounts.Remove): what it does to its folder, and first
/// how many changes made here are not uploaded and stay on this device only. It acts on the
/// account it was opened for, and closes when another is shown.
Kirigami.PromptDialog {
    id: dialog

    /// The account it was opened for (an AccountItem); null once that account is gone.
    property QtObject item: null
    /// How many changes made here are not uploaded (UnsentCount), read when the
    /// question is asked; 0 until it is answered, and when it cannot be read.
    property double unsentCount: 0

    /// What a question about the folder says first of `count` changes made here that are
    /// not uploaded.
    function unsentWarning(count) {
        return i18np("1 change made here is not uploaded yet: it stays on this device only, as a plain file.",
                     "%1 changes made here are not uploaded yet: they stay on this device only, as plain files.",
                     count);
    }

    function confirm() {
        if (item) {
            Accounts.removeAccount(item.path);
        }
        close();
    }

    title: item ? i18nc("@title:window", "Disconnect %1?", item.account.label) : ""
    subtitle: {
        const stays = item && item.sync.rootPath.length > 0
            ? i18n("KOneDrive stops syncing %1 and signs out of the account. The files downloaded so far, and your own, stay there as plain files; the rest are only in OneDrive. Nothing in OneDrive is deleted.", item.sync.rootPath)
            : i18n("KOneDrive signs out of this account and forgets it on this device. Nothing in OneDrive is deleted.");
        return unsentCount > 0 ? unsentWarning(unsentCount) + "\n\n" + stays : stays;
    }
    onAboutToShow: {
        item = Current.item;
        unsentCount = 0;
        if (item) {
            item.sync.readUnsentCount();
        }
    }
    dialogType: Kirigami.PromptDialog.Warning
    maximumWidth: Math.min(absoluteMaximumWidth, Kirigami.Units.gridUnit * 30)
    standardButtons: Kirigami.Dialog.NoButton
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

    Connections {
        target: dialog.item ? dialog.item.sync : null
        function onUnsentCountRead(count) {
            dialog.unsentCount = count;
        }
    }
    // Another account shown, or this one gone: not the question asked.
    Connections {
        target: Current
        function onChanged() {
            dialog.close();
        }
    }
}
