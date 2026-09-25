import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Dialogs
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The account chosen in the switcher: its name, whether changes made here
/// are uploaded (its mode), signing in and out, its quota, its folder
/// (choose, or forget), what is never uploaded and this computer's name for
/// copies, and removing it — or that the service is not running.
FormCard.FormCardPage {
    id: page

    readonly property var account: Current.account
    readonly property var sync: Current.sync
    // Current.account and Current.sync change together, but these bindings
    // may see one before the other: each checks both.
    readonly property bool available: account !== null && sync !== null && account.serviceAvailable && sync.serviceAvailable
    readonly property bool signedOut: available && account !== null && account.state === "signed-out"
    readonly property bool signingIn: available && account !== null && account.state === "signing-in"
    readonly property bool signedIn: available && account !== null && account.state === "signed-in"
    readonly property bool hasFolder: available && sync !== null && sync.rootPath.length > 0
    readonly property bool pending: available && sync !== null && sync.pendingFolder.length > 0
    /// A folder that shows OneDrive: the one kind anything is uploaded from.
    readonly property bool oneDrive: hasFolder && sync.rootSource === "onedrive"
    readonly property string accountError: account ? (account.actionError.length > 0 ? account.actionError : account.lastError) : ""
    readonly property var window: QQC2.ApplicationWindow.window

    /// "Upload changes made on this computer" turned on or off. On explains
    /// first, and signs in only once the user goes on; off switches at once,
    /// and asks only if changes still wait to be uploaded.
    function setUploads(on) {
        if (on) {
            uploadDialog.open();
        } else if (account) {
            account.setMode("read-only", false);
        }
    }

    /// Sign In calls this once the account it just added is signed in,
    /// named and chosen: "a sign-in exists to sync something".
    function openFolderPicker() {
        if (Current.item) {
            folderDialog.openFor(Current.item);
        }
    }

    objectName: "accountPage"
    title: window ? window.accountTitle(i18nc("@title", "Account")) : i18nc("@title", "Account")

    Connections {
        target: page.account
        function onPendingUploadsRefused() {
            dropUploadsDialog.open();
        }
    }

    Kirigami.InlineMessage {
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
    // A Remove of this account that was refused; one refused for want of
    // the helper says what to do, and can be tried again.
    Kirigami.InlineMessage {
        objectName: "removeError"
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: {
            if (Daemon.removeNeedsHelper) {
                return i18n("The account was not removed: its folder can be forgotten only through the konedrive helper, which is not available. %1", Daemon.helperInstruction);
            }
            if (Daemon.removeWaitsForUploads) {
                return i18n("The account was not removed: changes made on this computer have not been uploaded yet, and removing it now would lose them. Wait until they are uploaded, or turn uploading off for this account and choose not to upload them; then remove it.");
            }
            return i18n("The account was not removed: %1", Daemon.removeError);
        }
        visible: page.available && page.account !== null && Daemon.removeFailedPath === page.account.path
        actions: [
            Kirigami.Action {
                text: i18nc("@action:button", "Try Again")
                icon.name: "view-refresh"
                visible: Daemon.removeNeedsHelper
                enabled: Daemon.removing.length === 0
                onTriggered: Accounts.removeAccount(page.account.path)
            }
        ]
    }

    // Daemon not reachable
    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: !page.available

        FormCard.FormTextDelegate {
            text: i18n("The KOneDrive service is not running")
            description: i18n("Install it with scripts/dev-install.sh, then try again.")
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Try Again")
            icon.name: "view-refresh"
            // M8: the daemon that came back owns every object — all re-fetch.
            onClicked: Accounts.retry()
        }
    }

    // The account in KOneDrive: its name, and what it may do.
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
        // The mode: on is read-write. It shows the mode the account runs in
        // (Account1.Mode), or the one a switch under way goes to.
        FormCard.FormSwitchDelegate {
            id: uploadSwitch
            objectName: "uploadSwitch"

            readonly property bool uploading: page.account !== null
                && (page.account.switchingTo.length > 0 ? page.account.switchingTo === "read-write" : page.account.mode === "read-write")

            text: i18n("Upload changes made on this computer")
            description: {
                if (!page.account) {
                    return "";
                }
                if (page.account.modeSignInPending) {
                    return i18n("Waiting for you to sign in in your browser and allow KOneDrive to change your files.");
                }
                if (page.account.switchingTo === "read-write") {
                    return i18n("Starting the sign-in…");
                }
                if (page.account.switchingTo === "read-only") {
                    return i18n("Turning off…");
                }
                if (page.account.mode === "read-write") {
                    return i18n("Files you add, change, move or delete in the OneDrive folder are uploaded to OneDrive.");
                }
                if (!page.signedIn) {
                    return i18n("The OneDrive folder is read-only on this computer. Sign in to turn this on.");
                }
                return i18n("The OneDrive folder is read-only on this computer: nothing made here is uploaded.");
            }
            checked: uploading
            // FormSwitchDelegate's inner switch writes `checked` back, which
            // ends a binding on it: so the mode is also written on each change.
            onUploadingChanged: checked = uploading
            enabled: page.signedIn && page.account !== null && page.account.switchingTo.length === 0
            onToggled: {
                const wanted = checked;
                // The daemon's answer decides; until then the switch shows the mode.
                checked = uploading;
                page.setUploads(wanted);
            }
        }
        FormCard.FormButtonDelegate {
            visible: page.account !== null && page.account.modeSignInPending
            text: i18nc("@action:button", "Copy Sign-In Link")
            icon.name: "edit-copy"
            onClicked: page.account.copySignInUrl()
        }
        FormCard.FormButtonDelegate {
            objectName: "cancelModeSwitch"
            visible: page.account !== null && page.account.modeSignInPending
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onClicked: page.account.cancelModeSwitch()
        }
    }

    // Signed out
    FormCard.FormHeader {
        visible: page.signedOut
        title: i18nc("@title:group", "Sign In")
    }
    FormCard.FormCard {
        visible: page.signedOut

        FormCard.FormTextDelegate {
            visible: Daemon.clientId.length === 0
            text: i18n("A client ID is needed first")
            description: i18n("Register an application in Microsoft Entra and enter its Application (client) ID in Settings, under Advanced. README.md explains the steps.")
        }
        FormCard.FormButtonDelegate {
            visible: Daemon.clientId.length === 0
            text: i18nc("@action:button", "Open Settings")
            icon.name: "settings-configure"
            onClicked: page.window.showPage("settings")
        }
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Sign In to OneDrive")
            icon.name: "go-next"
            enabled: Daemon.clientId.length > 0
            onClicked: page.account.signIn()
        }
    }

    // Signing in
    FormCard.FormHeader {
        visible: page.signingIn
        title: i18nc("@title:group", "Signing In")
    }
    FormCard.FormCard {
        visible: page.signingIn

        FormCard.FormTextDelegate {
            text: i18n("Finish signing in in your browser")
            description: page.account ? page.account.signInUrl : ""
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Copy Sign-In Link")
            icon.name: "edit-copy"
            enabled: page.account !== null && page.account.signInUrl.length > 0
            onClicked: page.account.copySignInUrl()
        }
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onClicked: page.account.cancelSignIn()
        }
    }

    // Signed in
    FormCard.FormHeader {
        visible: page.signedIn
        title: i18nc("@title:group", "Microsoft Account")
    }
    FormCard.FormCard {
        visible: page.signedIn

        FormCard.FormTextDelegate {
            text: page.account && page.account.displayName.length > 0 ? page.account.displayName : i18n("Loading…")
            description: page.account ? page.account.email : ""
        }
        FormCard.FormDelegateSeparator {}
        FormCard.AbstractFormDelegate {
            background: null
            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing

                QQC2.Label {
                    Layout.fillWidth: true
                    text: page.account ? i18n("%1 of %2 used",
                                              Qt.locale().formattedDataSize(page.account.quotaUsed),
                                              Qt.locale().formattedDataSize(page.account.quotaTotal)) : ""
                }
                QQC2.ProgressBar {
                    Layout.fillWidth: true
                    from: 0
                    to: page.account ? Math.max(1, page.account.quotaTotal) : 1
                    value: page.account ? page.account.quotaUsed : 0
                }
            }
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Refresh")
            icon.name: "view-refresh"
            onClicked: page.account.refreshAccountInfo()
        }
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Sign Out")
            icon.name: "system-log-out"
            onClicked: page.account.signOut()
        }
    }

    FolderDialog {
        id: folderDialog

        /// The account it was opened for (an AccountItem); null once that account is gone.
        property QtObject item: null

        function openFor(account) {
            item = account;
            open();
        }

        title: i18nc("@title:window", "Choose an Empty Folder for OneDrive")
        onAccepted: {
            if (item) {
                item.sync.chooseFolder(selectedFolder);
            }
            item = null;
        }
        onRejected: item = null
    }

    // The OneDrive folder: asked for once signed in; a folder already there
    // shows whatever the sign-in.
    FormCard.FormHeader {
        visible: page.signedIn || page.hasFolder || page.pending
        title: i18nc("@title:group", "OneDrive Folder")
    }
    FormCard.FormCard {
        visible: page.signedIn || page.hasFolder || page.pending

        FormCard.FormTextDelegate {
            visible: page.signedIn && !page.hasFolder && !page.pending
            text: i18n("No folder yet")
            description: i18n("Choose an empty folder. Your OneDrive appears in it; files download when you open them.")
        }
        FormCard.FormButtonDelegate {
            objectName: "chooseFolder"
            visible: page.signedIn && !page.hasFolder && !page.pending
            text: i18nc("@action:button", "Choose Folder…")
            icon.name: "folder-open"
            onClicked: folderDialog.openFor(Current.item)
        }

        // RegisterRoot refused NoHelper: the window never registers a folder
        // without interception (I3) — an unhydrated file would read as zeros
        // for good. Try Again retries RegisterRoot; Cancel gives up on it.
        FormCard.FormTextDelegate {
            visible: page.pending
            text: {
                switch (Daemon.helperState) {
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
            visible: page.pending && Daemon.helperInstruction.length > 0
            text: Daemon.helperInstruction
            textItem.textFormat: Text.PlainText
            textItem.wrapMode: Text.Wrap
        }
        FormCard.FormButtonDelegate {
            visible: page.pending
            text: i18nc("@action:button", "Try Again")
            icon.name: "view-refresh"
            onClicked: page.sync.retryRegistration()
        }
        FormCard.FormButtonDelegate {
            visible: page.pending
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onClicked: page.sync.cancelPending()
        }

        FormCard.FormTextDelegate {
            visible: page.hasFolder
            text: page.sync ? page.sync.rootPath : ""
            description: page.sync && page.sync.rootSource === "onedrive" ? i18n("Shows your OneDrive") : i18n("Filled from a local folder")
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
            onClicked: page.sync.forget()
        }
    }

    // This account's own settings for uploading (the Settings page is the whole app's).
    FormCard.FormHeader {
        visible: page.oneDrive
        title: i18nc("@title:group", "Uploading")
    }
    FormCard.FormCard {
        objectName: "uploadSettings"
        visible: page.oneDrive

        FormCard.FormTextDelegate {
            objectName: "machineName"
            text: page.sync ? i18n("This computer's name: %1", page.sync.machineName) : ""
            description: page.sync ? i18n("When a file changes both here and in OneDrive, your version is kept beside it, named after this computer: Report-%1.docx. It is machine_name in ~/.config/konedrive/config.toml.", page.sync.machineName) : ""
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: i18n("Never uploaded")
            description: i18n("Files of your own whose names match one of these stay here and are never uploaded. A pattern matches a name, not a path: * stands for anything, ? for one character. Files that came from OneDrive are not affected.")
        }
        Repeater {
            model: page.sync ? page.sync.ignorePatterns : []
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
            label: i18n("Add a pattern")
            placeholderText: "*.tmp"
            onAccepted: addPattern.clicked()
        }
        FormCard.FormButtonDelegate {
            id: addPattern
            objectName: "addIgnorePattern"
            text: i18nc("@action:button", "Add")
            icon.name: "list-add"
            enabled: ignoreField.text.trim().length > 0
            onClicked: {
                page.sync.addIgnorePattern(ignoreField.text);
                ignoreField.text = "";
            }
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing * 2
        visible: page.available

        FormCard.FormButtonDelegate {
            objectName: "removeAccountButton"
            text: i18nc("@action:button", "Remove Account…")
            description: i18n("Signs out and forgets this account on this computer. Nothing in OneDrive is deleted.")
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
                    text: i18n("Removing the account…")
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

        title: i18nc("@title:window", "Rename Account")
        subtitle: i18n("The name KOneDrive shows for this account.")
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

    // Before a switch to read-write: what it means, and that a sign-in follows.
    Kirigami.PromptDialog {
        id: uploadDialog
        objectName: "uploadDialog"

        /// The account it was opened for (an AccountItem); null once that account is gone.
        property QtObject item: null

        function confirm() {
            if (item) {
                item.account.setMode("read-write", false);
            }
            close();
        }

        title: i18nc("@title:window", "Upload Changes Made on This Computer?")
        subtitle: {
            const signIn = i18n("Your browser opens so that you can sign in to Microsoft again, this time allowing KOneDrive to change the files in your OneDrive.");
            const meaning = item && item.sync.rootPath.length > 0
                ? i18n("From then on, what you do in %1 is uploaded: new files, edits, renames, moves and deletions. It shows in OneDrive on the web and on your other devices, and deleted files go to OneDrive's recycle bin.", item.sync.rootPath)
                : i18n("From then on, what you do in the OneDrive folder is uploaded: new files, edits, renames, moves and deletions. It shows in OneDrive on the web and on your other devices, and deleted files go to OneDrive's recycle bin.");
            return signIn + "\n\n" + meaning;
        }
        onAboutToShow: item = Current.item
        maximumWidth: Math.min(absoluteMaximumWidth, Kirigami.Units.gridUnit * 30)
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: i18nc("@action:button", "Continue in Browser")
                icon.name: "go-next"
                enabled: uploadDialog.item !== null
                onTriggered: uploadDialog.confirm()
            },
            Kirigami.Action {
                text: i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: uploadDialog.close()
            }
        ]

        // Another account shown, or this one gone: not the question asked.
        Connections {
            target: Current
            function onChanged() {
                uploadDialog.close();
            }
        }
    }

    // A switch to read-only refused because changes still wait to be uploaded.
    Kirigami.PromptDialog {
        id: dropUploadsDialog
        objectName: "dropUploadsDialog"

        /// The account it was opened for (an AccountItem); null once that account is gone.
        property QtObject item: null

        function confirm() {
            if (item) {
                item.account.setMode("read-only", true);
            }
            close();
        }

        title: i18nc("@title:window", "Stop Uploading?")
        subtitle: item && item.sync.pendingCount > 0
            ? i18np("1 change made on this computer has not been uploaded yet. If you turn uploading off now, it never will be: the file stays here as it is, but OneDrive does not get the change.",
                    "%1 changes made on this computer have not been uploaded yet. If you turn uploading off now, they never will be: the files stay here as they are, but OneDrive does not get these changes.",
                    item.sync.pendingCount)
            : i18n("Some changes made on this computer have not been uploaded yet. If you turn uploading off now, they never will be: the files stay here as they are, but OneDrive does not get these changes.")
        onAboutToShow: item = Current.item
        dialogType: Kirigami.PromptDialog.Warning
        maximumWidth: Math.min(absoluteMaximumWidth, Kirigami.Units.gridUnit * 30)
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: i18nc("@action:button", "Turn Off Without Uploading")
                icon.name: "process-stop"
                enabled: dropUploadsDialog.item !== null
                onTriggered: dropUploadsDialog.confirm()
            },
            Kirigami.Action {
                text: i18nc("@action:button", "Keep Uploading")
                icon.name: "dialog-cancel"
                onTriggered: dropUploadsDialog.close()
            }
        ]

        Connections {
            target: Current
            function onChanged() {
                dropUploadsDialog.close();
            }
        }
    }

    Kirigami.PromptDialog {
        id: removeDialog
        objectName: "removeDialog"

        /// The account it was opened for (an AccountItem); null once that account is gone.
        property QtObject item: null

        function confirm() {
            if (item) {
                Accounts.removeAccount(item.path);
            }
            close();
        }

        title: item ? i18nc("@title:window", "Remove %1?", item.account.label) : ""
        subtitle: item && item.sync.rootPath.length > 0
            ? i18n("Your files stay in %1. Files that were never downloaded are left as empty placeholders.", item.sync.rootPath)
            : i18n("KOneDrive signs out of this account and forgets it on this computer. Nothing in OneDrive is deleted.")
        onAboutToShow: item = Current.item
        dialogType: Kirigami.PromptDialog.Warning
        maximumWidth: Math.min(absoluteMaximumWidth, Kirigami.Units.gridUnit * 30)
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: i18nc("@action:button", "Remove Account")
                icon.name: "list-remove-user"
                enabled: removeDialog.item !== null
                onTriggered: removeDialog.confirm()
            },
            Kirigami.Action {
                text: i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: removeDialog.close()
            }
        ]

        // Another account shown, or this one gone: not the question asked.
        Connections {
            target: Current
            function onChanged() {
                removeDialog.close();
            }
        }
    }
}
