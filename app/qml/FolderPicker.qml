import QtCore
import QtQuick
import QtQuick.Dialogs
import org.konedrive.app

/// The dialog that picks an account's folder: opened by itself once a OneDrive is added, by
/// Choose Folder… on Status, and by Use Another Folder… of a folder that stopped. The folder
/// picked goes to FolderChoice; one that cannot be the account's opens the dialog again,
/// headed by why, which Status also shows.
FolderDialog {
    id: picker

    /// The account it is open for (an AccountItem); null once closed, or once that account is gone.
    property QtObject item: null

    /// Opens the dialog for `account`.
    function openFor(account) {
        item = account;
        title = i18nc("@title:window", "Choose an Empty Folder for OneDrive");
        open();
    }

    /// The folder picked for the account it is open for: given, or the dialog opened again
    /// with why it can't be.
    function pick(folder) {
        const account = item;
        item = null;
        if (account && !FolderChoice.choose(account, folder)) {
            item = account;
            title = FolderChoice.refusal;
            // Not while the dialog that answered is still closing.
            Qt.callLater(open);
        }
    }

    objectName: "folderPicker"
    currentFolder: StandardPaths.writableLocation(StandardPaths.HomeLocation)
    onAccepted: pick(selectedFolder)
    onRejected: item = null
}
