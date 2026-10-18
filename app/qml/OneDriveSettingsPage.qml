import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The settings of the OneDrive chosen in the sidebar — an account, its folder and how it
/// syncs, as one: its name and Microsoft account, how it syncs, its folder (or another one),
/// what it keeps on this device, the advanced settings (what is never uploaded, this
/// device's name for copies, signing out), and disconnecting it.
FormCard.FormCardPage {
    id: page

    readonly property var account: Current.account
    readonly property var sync: Current.sync
    // Current.account and Current.sync change together, but these bindings
    // may see one before the other: each checks both.
    readonly property bool available: account !== null && sync !== null && account.serviceAvailable && sync.serviceAvailable
    readonly property bool signedOut: available && account.state === "signed-out"
    readonly property bool signingIn: available && account.state === "signing-in"
    readonly property bool signedIn: available && account.state === "signed-in"
    readonly property bool hasFolder: available && sync.rootPath.length > 0
    readonly property string accountError: account ? (account.actionError.length > 0 ? account.actionError : account.lastError) : ""
    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "oneDriveSettingsPage"
    title: window ? window.accountTitle(i18nc("@title", "OneDrive Settings")) : i18nc("@title", "OneDrive Settings")

    Kirigami.InlineMessage {
        objectName: "accountError"
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: page.accountError
        visible: page.available && text.length > 0
    }
    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: page.sync ? page.sync.actionError : ""
        visible: page.available && text.length > 0
    }
    // A Remove of this account that was refused.
    Kirigami.InlineMessage {
        objectName: "removeError"
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: i18n("This OneDrive was not disconnected: %1", Daemon.removeError)
        visible: page.available && page.account !== null && Daemon.removeFailedPath === page.account.path
    }

    ServiceCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: !page.available
    }

    // The account: its name here, and the Microsoft account behind it.
    FormCard.FormHeader {
        visible: page.available
        title: i18nc("@title:group", "Account")
    }
    FormCard.FormCard {
        visible: page.available

        FormCard.FormTextDelegate {
            text: i18nc("@label", "Name")
            description: page.account ? page.account.label : ""
            trailing: QQC2.Button {
                objectName: "renameButton"
                text: i18nc("@action:button", "Rename…")
                icon.name: "edit-rename"
                onClicked: renameDialog.open()
            }
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            visible: page.signedIn
            text: page.account && page.account.displayName.length > 0 ? page.account.displayName : i18n("Microsoft account")
            description: page.account ? page.account.email : ""
            trailing: QQC2.Button {
                text: i18nc("@action:button", "Refresh")
                icon.name: "view-refresh"
                display: QQC2.AbstractButton.IconOnly
                QQC2.ToolTip.text: i18nc("@info:tooltip", "Read the name and the storage again")
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                onClicked: page.account.refreshAccountInfo()
            }
        }
        FormCard.FormTextDelegate {
            visible: page.signedOut
            text: i18n("Signed out of OneDrive")
            description: i18n("Nothing syncs until you sign in again.")
        }
        FormCard.FormButtonDelegate {
            visible: page.signedOut
            text: i18nc("@action:button", "Sign In Again")
            icon.name: "go-next"
            onClicked: page.account.signIn()
        }
        FormCard.FormTextDelegate {
            visible: page.signingIn
            text: i18n("Finish signing in in your browser")
            description: page.account ? page.account.signInUrl : ""
        }
        FormCard.FormButtonDelegate {
            visible: page.signingIn
            text: i18nc("@action:button", "Copy Sign-In Link")
            icon.name: "edit-copy"
            enabled: page.account !== null && page.account.signInUrl.length > 0
            onClicked: page.account.copySignInUrl()
        }
        FormCard.FormButtonDelegate {
            visible: page.signingIn
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onClicked: page.account.cancelSignIn()
        }
    }

    // How the folder syncs.
    FormCard.FormHeader {
        visible: page.hasFolder
        title: i18nc("@title:group", "Sync")
    }
    ModeCard {
        visible: page.hasFolder
    }

    // The folder: where the OneDrive shows up on this device.
    FormCard.FormHeader {
        visible: page.hasFolder
        title: i18nc("@title:group", "Folder")
    }
    FormCard.FormCard {
        visible: page.hasFolder

        FormCard.FormTextDelegate {
            text: page.sync ? page.sync.rootPathText : ""
            description: i18n("Your OneDrive on this device")
            leading: Kirigami.Icon {
                source: "folder-cloud"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
            trailing: QQC2.Button {
                text: i18nc("@action:button", "Open Folder")
                icon.name: "system-file-manager"
                onClicked: page.sync.openFolder()
            }
        }
        FormCard.FormButtonDelegate {
            objectName: "anotherFolderButton"
            text: i18nc("@action:button", "Use Another Folder…")
            description: i18n("Your OneDrive moves to another empty folder; this one keeps its files as plain files.")
            icon.name: "folder-open"
            onClicked: anotherFolderDialog.open()
        }
    }

    // What the folder keeps on this device: the account's own sync settings (issues #80,
    // #217), the daemon's, written to config.toml; each control shows what it says, and a
    // change goes through it. Pausing on metered connections and on battery is the whole
    // app's, on the Settings page (issue #95).
    FormCard.FormHeader {
        visible: page.hasFolder
        title: i18nc("@title:group", "On This Device")
    }
    FormCard.FormCard {
        objectName: "deviceSettings"
        visible: page.hasFolder

        FormCard.FormSwitchDelegate {
            id: thumbnailsSwitch
            objectName: "thumbnailsSwitch"

            readonly property bool on: page.sync !== null && page.sync.thumbnails

            text: i18n("Previews without downloading")
            description: i18n("Dolphin shows previews of images, videos, PDFs and office documents from the small thumbnails OneDrive makes.")
            checked: on
            // The inner switch writes `checked` back, ending the binding: kept in step here.
            onOnChanged: checked = on
            onToggled: {
                const wanted = checked;
                checked = on;
                page.sync.setThumbnails(wanted);
            }
        }
        FormCard.FormTextDelegate {
            objectName: "thumbnailsOffLine"
            visible: !thumbnailsSwitch.on
            text: i18n("While Dolphin shows previews, it downloads a file that is only in OneDrive in full to make its preview.")
            textItem.wrapMode: Text.Wrap
            leading: Kirigami.Icon {
                source: "dialog-warning"
                implicitWidth: Kirigami.Units.iconSizes.small
                implicitHeight: Kirigami.Units.iconSizes.small
            }
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSpinBoxDelegate {
            id: freeSpaceReserveBox
            objectName: "freeSpaceReserveBox"

            readonly property int kept: page.sync !== null ? page.sync.freeSpaceReserve : 0

            label: i18n("Keep at least this much free space on this device, in GiB")
            from: 0
            to: 100000
            value: kept
            // The inner box writes `value` back, ending the binding: kept in step here.
            onKeptChanged: value = kept
            onValueChanged: {
                if (page.sync !== null && value !== kept) {
                    page.sync.setFreeSpaceReserve(value);
                }
            }
        }
        FormCard.FormTextDelegate {
            objectName: "freeSpaceReserveLine"
            text: i18n("A file always kept on this device is not downloaded while its download would leave less free space than this.")
            textItem.wrapMode: Text.Wrap
        }
    }

    // What is seldom changed.
    FormCard.FormHeader {
        visible: page.available
        title: i18nc("@title:group", "Advanced")
    }
    FormCard.FormCard {
        objectName: "advancedSettings"
        visible: page.available

        FormCard.FormTextDelegate {
            visible: page.hasFolder
            text: i18n("Never uploaded")
            description: i18n("Files of your own whose names match one of these stay on this device and are never uploaded. A pattern matches a name, not a path: * stands for anything, ? for one character. Files that came from OneDrive are not affected.")
            textItem.wrapMode: Text.Wrap
        }
        Repeater {
            model: page.hasFolder && page.sync ? page.sync.ignorePatterns : []
            delegate: FormCard.FormTextDelegate {
                required property string modelData
                text: modelData
                trailing: QQC2.Button {
                    text: i18nc("@action:button", "Remove")
                    icon.name: "list-remove"
                    display: QQC2.AbstractButton.IconOnly
                    QQC2.ToolTip.text: text
                    QQC2.ToolTip.visible: hovered
                    onClicked: page.sync.removeIgnorePattern(modelData)
                }
            }
        }
        FormCard.FormTextFieldDelegate {
            id: ignoreField
            objectName: "ignoreField"
            visible: page.hasFolder
            label: i18n("Add a pattern")
            placeholderText: "*.tmp"
            onAccepted: addPattern.clicked()
        }
        FormCard.FormButtonDelegate {
            id: addPattern
            objectName: "addIgnorePattern"
            visible: page.hasFolder
            text: i18nc("@action:button", "Add")
            icon.name: "list-add"
            enabled: ignoreField.text.trim().length > 0
            onClicked: {
                page.sync.addIgnorePattern(ignoreField.text);
                ignoreField.text = "";
            }
        }
        FormCard.FormDelegateSeparator {
            visible: page.hasFolder
        }
        FormCard.FormTextDelegate {
            objectName: "machineName"
            visible: page.hasFolder
            text: page.sync ? i18n("This device's name for copies: %1", page.sync.machineName) : ""
            description: page.sync ? i18n("When a file changes both here and in OneDrive, your version is kept beside it, named after this device: Report-%1.docx. It is machine_name in ~/.config/konedrive/config.toml.", page.sync.machineName) : ""
            textItem.wrapMode: Text.Wrap
        }
        FormCard.FormDelegateSeparator {
            visible: page.hasFolder && page.signedIn
        }
        FormCard.FormButtonDelegate {
            objectName: "signOutButton"
            visible: page.signedIn
            text: i18nc("@action:button", "Sign Out")
            description: i18n("The folder stays with the files already downloaded, but nothing syncs until you sign in again.")
            icon.name: "system-log-out"
            onClicked: page.account.signOut()
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing * 2
        visible: page.available

        FormCard.FormButtonDelegate {
            objectName: "removeAccountButton"
            text: i18nc("@action:button", "Disconnect This OneDrive…")
            description: i18n("KOneDrive stops syncing this folder and signs out of the account. Nothing in OneDrive is deleted.")
            icon.name: "list-remove-user"
            // One Remove at a time: it forgets the folder first, which can take a while.
            enabled: Daemon.removing.length === 0
            onClicked: removeDialog.open()
        }
        FormCard.AbstractFormDelegate {
            objectName: "removingRow"
            visible: page.account !== null && Daemon.removing === page.account.path
            background: null
            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing
                QQC2.BusyIndicator {
                    running: parent.visible
                    implicitWidth: Kirigami.Units.iconSizes.medium
                    implicitHeight: Kirigami.Units.iconSizes.medium
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    text: i18n("Disconnecting…")
                }
            }
        }
    }

    Kirigami.PromptDialog {
        id: renameDialog
        objectName: "renameDialog"

        /// The account it was opened for (an AccountItem); null once that account is gone.
        property QtObject item: null
        readonly property string problem: item ? Accounts.labelProblem(labelField.text, item.path) : ""
        readonly property bool ready: item !== null && problem.length === 0 && labelField.text.trim() !== item.account.label

        function rename() {
            if (ready) {
                item.account.setLabel(labelField.text);
                close();
            }
        }

        title: i18nc("@title:window", "Rename")
        subtitle: i18n("The name KOneDrive shows for this OneDrive, in the sidebar, the tray and Places.")
        standardButtons: Kirigami.Dialog.NoButton
        onAboutToShow: item = Current.item
        onOpened: {
            labelField.text = item ? item.account.label : "";
            labelField.selectAll();
            labelField.forceActiveFocus();
        }

        // Another account shown, or this one gone: not the question asked.
        Connections {
            target: Current
            function onChanged() {
                renameDialog.close();
            }
        }

        QQC2.TextField {
            id: labelField
            Layout.fillWidth: true
            onAccepted: renameDialog.rename()
        }
        QQC2.Label {
            Layout.fillWidth: true
            visible: labelField.text.trim().length > 0 && renameDialog.problem.length > 0
            text: renameDialog.problem
            wrapMode: Text.Wrap
            font: Kirigami.Theme.smallFont
            color: Kirigami.Theme.negativeTextColor
        }

        customFooterActions: [
            Kirigami.Action {
                text: i18nc("@action:button", "Rename")
                icon.name: "edit-rename"
                enabled: renameDialog.ready
                onTriggered: renameDialog.rename()
            },
            Kirigami.Action {
                text: i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: renameDialog.close()
            }
        ]
    }

    // Another folder: this one is forgotten (its files stay, as plain files), and the
    // account's Status offers it a new one.
    Kirigami.PromptDialog {
        id: anotherFolderDialog
        objectName: "anotherFolderDialog"

        /// The account it was opened for (an AccountItem); null once that account is gone.
        property QtObject item: null
        /// How many changes made here are not uploaded (UnsentCount), read when the question
        /// is asked; 0 until it is answered, and when it cannot be read.
        property double unsentCount: 0

        function confirm() {
            if (item) {
                item.sync.forget();
            }
            close();
        }

        title: i18nc("@title:window", "Use Another Folder?")
        subtitle: {
            const leaves = item && item.sync.rootPath.length > 0
                ? i18n("KOneDrive stops syncing %1 and leaves the files downloaded so far, and your own, there as plain files. Then you choose an empty folder: your OneDrive shows up there, and files download again when you open them.", item.sync.rootPath)
                : "";
            return unsentCount > 0 ? removeDialog.unsentWarning(unsentCount) + "\n\n" + leaves : leaves;
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
                text: i18nc("@action:button", "Choose Another Folder…")
                icon.name: "folder-open"
                enabled: anotherFolderDialog.item !== null
                onTriggered: anotherFolderDialog.confirm()
            },
            Kirigami.Action {
                text: i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: anotherFolderDialog.close()
            }
        ]

        Connections {
            target: anotherFolderDialog.item ? anotherFolderDialog.item.sync : null
            function onUnsentCountRead(count) {
                anotherFolderDialog.unsentCount = count;
            }
        }
        // Another account shown, or this one gone: not the question asked.
        Connections {
            target: Current
            function onChanged() {
                anotherFolderDialog.close();
            }
        }
    }

    DisconnectDialog {
        id: removeDialog
        objectName: "removeDialog"
    }
}
