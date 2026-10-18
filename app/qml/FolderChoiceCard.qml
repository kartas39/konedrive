import QtCore
import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Dialogs
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The last step of adding a OneDrive, on Status while the account shown has no folder: the
/// folder it gets (FolderChoice: the one offered, or another chosen), whether it syncs
/// two-way, and Continue. Two-way waits for the browser first, with its link and a way out.
/// Cancel disconnects the account: one with no folder is not kept.
ColumnLayout {
    id: card

    readonly property var account: Current.account
    /// The choice made here, applied by Continue: the account's mode until it is changed.
    property bool twoWay: account !== null && account.mode === "read-write"
    /// The switch to two-way that Continue asked for waits for the browser.
    readonly property bool waiting: account !== null && account.switchingTo === "read-write"

    objectName: "folderChoiceCard"
    spacing: 0
    // The inner switch writes `checked` back, ending its binding: kept in step here.
    onTwoWayChanged: twoWaySwitch.checked = twoWay

    // Another account: its own mode is the choice.
    Connections {
        target: Current
        function onChanged() {
            card.twoWay = Qt.binding(() => card.account !== null && card.account.mode === "read-write");
        }
    }

    // The dialogs are the layout's, not the card's: a card works out its separators from
    // what it holds, and a dialog's items come and go.
    FormCard.FormCard {
        Layout.fillWidth: true

        FormCard.FormTextDelegate {
            text: i18n("Where should your OneDrive appear?")
            description: i18n("It shows up in a folder on this device, and files download when you open them.")
            textItem.wrapMode: Text.Wrap
            leading: Kirigami.Icon {
                source: "folder-cloud"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            objectName: "offeredFolder"
            text: FolderChoice.folder.length > 0 ? FolderChoice.folderText : i18n("Choose a folder")
            description: i18n("An empty folder, made if it is not there")
            trailing: QQC2.Button {
                objectName: "chooseAnotherButton"
                text: FolderChoice.folder.length > 0 ? i18nc("@action:button", "Choose Another…") : i18nc("@action:button", "Choose…")
                icon.name: "folder-open"
                enabled: !card.waiting
                onClicked: folderDialog.open()
            }
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            id: twoWaySwitch
            objectName: "twoWaySwitch"
            text: i18n("Two-way sync")
            description: {
                if (!card.twoWay) {
                    return i18n("Files from OneDrive arrive and open as usual, but can't be changed here, and nothing is uploaded. You can turn on two-way sync later.");
                }
                const uploads = i18n("What you add, change, rename, move or delete in the folder is uploaded to OneDrive and shows on your other devices.");
                return card.account !== null && card.account.mode === "read-write" ? uploads
                    : uploads + " " + i18n("Your browser opens once more to allow KOneDrive to change your files.");
            }
            enabled: !card.waiting
            checked: card.twoWay
            onToggled: card.twoWay = checked
        }
        FormCard.FormTextDelegate {
            visible: card.waiting
            text: i18n("Waiting for you to allow KOneDrive to change your files in your browser…")
            textItem.wrapMode: Text.Wrap
            icon.name: "view-refresh"
        }
        FormCard.FormButtonDelegate {
            visible: card.waiting
            enabled: card.account !== null && card.account.signInUrl.length > 0
            text: i18nc("@action:button", "Copy Sign-In Link")
            icon.name: "edit-copy"
            onClicked: card.account.copySignInUrl()
        }
        FormCard.FormButtonDelegate {
            objectName: "cancelTwoWayButton"
            visible: card.waiting
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onClicked: card.account.cancelModeSwitch()
        }
        FormCard.FormDelegateSeparator {
            visible: !card.waiting
        }
        FormCard.FormButtonDelegate {
            objectName: "continueButton"
            visible: !card.waiting
            enabled: FolderChoice.folder.length > 0
            text: i18nc("@action:button", "Continue")
            icon.name: "go-next"
            onClicked: FolderChoice.choose(card.twoWay)
        }
        FormCard.FormButtonDelegate {
            objectName: "cancelSetUpButton"
            visible: !card.waiting
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onClicked: cancelDialog.open()
        }
    }

    FolderDialog {
        id: folderDialog
        objectName: "folderDialog"
        title: i18nc("@title:window", "Choose an Empty Folder for OneDrive")
        currentFolder: StandardPaths.writableLocation(StandardPaths.HomeLocation)
        onAccepted: FolderChoice.setFolder(selectedFolder)
    }

    DisconnectDialog {
        id: cancelDialog
        objectName: "cancelSetUpDialog"
    }
}
