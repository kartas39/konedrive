import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Dialogs
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The folder (choose, or forget), start at login, the client ID, and Quit.
FormCard.FormCardPage {
    id: page

    readonly property bool available: Account.serviceAvailable && Sync.serviceAvailable
    readonly property bool signedOut: available && Account.state === "signed-out"
    readonly property bool hasFolder: available && Sync.rootPath.length > 0
    readonly property bool choosing: available && !hasFolder && Sync.pendingFolder.length === 0

    objectName: "settingsPage"
    title: i18nc("@title", "Settings")

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: Sync.actionError
        visible: text.length > 0
    }

    FolderDialog {
        id: folderDialog
        title: i18nc("@title:window", "Choose an Empty Folder for OneDrive")
        onAccepted: Sync.chooseFolder(selectedFolder)
    }

    // The OneDrive folder
    FormCard.FormHeader {
        title: i18nc("@title:group", "OneDrive Folder")
    }
    FormCard.FormCard {
        FormCard.FormTextDelegate {
            visible: !page.available
            text: i18n("The KOneDrive service is not running")
            description: i18n("A folder can be chosen once it runs.")
        }
        FormCard.FormTextDelegate {
            visible: page.choosing
            text: i18n("No folder yet")
            description: i18n("Choose an empty folder. Your OneDrive appears in it; files download when you open them.")
        }
        FormCard.FormButtonDelegate {
            visible: page.choosing
            text: i18nc("@action:button", "Choose Folder…")
            icon.name: "folder-open"
            onClicked: folderDialog.open()
        }

        // RegisterRoot refused NoHelper: the window never registers a folder
        // without interception (I3) — an unhydrated file would read as zeros
        // for good. Try Again retries RegisterRoot; Cancel gives up on it.
        FormCard.FormTextDelegate {
            visible: page.available && Sync.pendingFolder.length > 0
            text: {
                switch (Sync.helperState) {
                case "stopped":
                    return i18n("The konedrive helper is not running");
                case "failed":
                    return i18n("The konedrive helper failed");
                case "unknown":
                    return i18n("The konedrive helper is not connected yet");
                default:
                    return i18n("The konedrive helper is not installed");
                }
            }
            description: i18n("Without it, nothing downloads a file when a program opens it, so KOneDrive will not add this folder yet.")
        }
        FormCard.FormTextDelegate {
            visible: page.available && Sync.pendingFolder.length > 0 && Sync.helperInstruction.length > 0
            text: Sync.helperInstruction
            textItem.textFormat: Text.PlainText
            textItem.wrapMode: Text.Wrap
        }
        FormCard.FormButtonDelegate {
            visible: page.available && Sync.pendingFolder.length > 0
            text: i18nc("@action:button", "Try Again")
            icon.name: "view-refresh"
            onClicked: Sync.retryRegistration()
        }
        FormCard.FormButtonDelegate {
            visible: page.available && Sync.pendingFolder.length > 0
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onClicked: Sync.cancelPending()
        }

        FormCard.FormTextDelegate {
            visible: page.hasFolder
            text: Sync.rootPath
            description: Sync.rootSource === "onedrive" ? i18n("Shows your OneDrive") : i18n("Filled from a local folder")
            leading: Kirigami.Icon {
                source: "folder-cloud"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.FormButtonDelegate {
            visible: page.hasFolder
            text: i18nc("@action:button", "Forget Folder")
            description: i18n("The files stay where they are; KOneDrive stops keeping them in step.")
            icon.name: "edit-delete-remove"
            onClicked: Sync.forget()
        }
    }

    // The app itself
    FormCard.FormHeader {
        title: i18nc("@title:group", "App")
    }
    FormCard.FormCard {
        FormCard.FormSwitchDelegate {
            id: startAtLogin
            objectName: "startAtLogin"
            text: i18n("Start at login")
            description: i18n("KOneDrive starts in the system tray when you log in. Notifications come from it, so with it closed nothing notifies.")
            checked: Autostart.enabled
            onToggled: {
                Autostart.enabled = checked;
                // The entry is the truth: after a failure the switch goes back.
                checked = Qt.binding(() => Autostart.enabled);
            }
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            type: Kirigami.MessageType.Error
            text: Autostart.error
            visible: text.length > 0
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            id: showDownloadProgress
            objectName: "showDownloadProgress"
            text: i18n("Show download progress")
            description: i18n("A long download shows its progress in Plasma, the same way Dolphin's copying does.")
            checked: DownloadProgress.enabled
            onToggled: DownloadProgress.enabled = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            id: showInPlaces
            objectName: "showInPlaces"
            text: i18n("Show in Places")
            description: i18n("Your OneDrive folder gets an entry in Dolphin's Places panel and in file dialogs.")
            checked: Places.enabled
            onToggled: Places.enabled = checked
        }
    }

    // Advanced
    FormCard.FormHeader {
        visible: page.available
        title: i18nc("@title:group", "Advanced")
    }
    FormCard.FormCard {
        visible: page.available

        FormCard.FormTextFieldDelegate {
            id: clientIdField
            label: i18n("Application (client) ID")
            placeholderText: "00000000-0000-0000-0000-000000000000"
            enabled: page.signedOut

            // FormTextFieldDelegate's inner TextField writes back to its own
            // `text` property (onTextChanged: root.text = text), which would
            // permanently sever a plain `text: Account.clientId` binding the
            // first time the field's text changes (including programmatically).
            // Re-sync explicitly instead, but never while the user is typing.
            Component.onCompleted: text = Account.clientId

            Connections {
                target: Account
                function onAccountChanged() {
                    if (!clientIdField.fieldActiveFocus) {
                        clientIdField.text = Account.clientId;
                    }
                }
            }
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Save Client ID")
            icon.name: "document-save"
            enabled: page.signedOut && clientIdField.text.trim() !== Account.clientId
            onClicked: Account.setClientId(clientIdField.text)
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing * 2

        FormCard.FormButtonDelegate {
            objectName: "quitButton"
            text: i18nc("@action:button", "Quit KOneDrive")
            description: i18n("Closes the window and the tray icon. Your files stay; nothing notifies until KOneDrive starts again.")
            icon.name: "application-exit"
            onClicked: Qt.quit()
        }
    }
}
