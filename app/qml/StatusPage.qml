import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The start page: how the chosen account's folder is doing, and what to do
/// about it. With no service, no account, no sign-in or no folder, it says so
/// and leads to what fixes it (Add Account…, or the Account page).
FormCard.FormCardPage {
    id: page

    readonly property var account: Current.account
    readonly property var sync: Current.sync
    readonly property var status: Current.status
    /// The service answers, and has no account yet.
    readonly property bool noAccount: Daemon.serviceAvailable && Accounts.count === 0
    // Current.account and Current.sync change together, but these bindings
    // may see one before the other: each checks both.
    readonly property bool available: account !== null && sync !== null && account.serviceAvailable && sync.serviceAvailable
    readonly property bool signedIn: available && account.state === "signed-in"
    readonly property bool hasFolder: available && sync !== null && status !== null && sync.rootPath.length > 0
    // A no-interception folder is one Free Up Space warning; a folder that
    // shows OneDrive but whose helper is not connected (including a legacy
    // folder still waiting to switch, which publishes RootState "error", not
    // "no-interception") is the same warning by a different RootState.
    readonly property bool freeUpUnsafe: (sync !== null && sync.rootState === "no-interception") || Daemon.helperState !== "connected"
    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "statusPage"
    title: window ? window.accountTitle(i18nc("@title", "Status")) : i18nc("@title", "Status")

    // Trouble that belongs to no account: config.toml unreadable, a failed migration.
    Kirigami.InlineMessage {
        objectName: "daemonError"
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: Daemon.lastError
        visible: Daemon.serviceAvailable && text.length > 0
    }

    // No account yet: the one way forward.
    FormCard.FormCard {
        objectName: "noAccountCard"
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.noAccount

        FormCard.FormPlaceholderMessageDelegate {
            text: i18n("Connect your OneDrive")
            explanation: i18n("Add your Microsoft account and choose a folder: your OneDrive appears in it, and files download when you open them.")
            icon.name: "folder-cloud"
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            objectName: "addAccountButton"
            text: i18nc("@action:button", "Add Account…")
            icon.name: "list-add-user"
            onClicked: page.window.addAccount()
        }
    }

    // The helper serves every account: nothing keeps a folder in step, and
    // nothing downloads on open, while this is anything but "connected"
    // (dbus/org.konedrive.Accounts1.xml).
    FormCard.FormCard {
        objectName: "helperCard"
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: Daemon.serviceAvailable && Daemon.helperTrouble && !page.noAccount

        FormCard.FormTextDelegate {
            text: i18n("The helper is not available")
            description: i18n("Files are not kept in step, and nothing downloads when it is opened.")
            leading: Kirigami.Icon {
                source: "dialog-warning"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.AbstractFormDelegate {
            visible: Daemon.helperInstruction.length > 0
            background: null
            contentItem: TextEdit {
                text: Daemon.helperInstruction
                wrapMode: Text.Wrap
                textFormat: TextEdit.PlainText
                readOnly: true
                selectByMouse: true
                color: Kirigami.Theme.textColor
                font: Kirigami.Theme.defaultFont
            }
        }
        FormCard.FormButtonDelegate {
            objectName: "helperCheckAgain"
            text: i18nc("@action:button", "Check Again")
            icon.name: "view-refresh"
            onClicked: Daemon.retry()
        }
    }

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: page.sync ? page.sync.actionError : ""
        visible: text.length > 0
    }

    // The service is not running (and so there is no account to show).
    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: !Daemon.serviceAvailable && page.account === null

        FormCard.FormTextDelegate {
            text: i18n("The KOneDrive service is not running")
            description: i18n("Install it with scripts/dev-install.sh, then try again.")
            leading: Kirigami.Icon {
                source: "state-offline"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Try Again")
            icon.name: "view-refresh"
            onClicked: Accounts.retry()
        }
    }

    // The folder and the status line, or what stands in the way.
    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.account !== null

        FormCard.FormTextDelegate {
            text: page.hasFolder && page.sync ? page.sync.rootPath : (page.status ? page.status.text : "")
            description: {
                if (page.hasFolder && page.status) {
                    return page.status.text;
                }
                if (!page.available) {
                    return i18n("Install it with scripts/dev-install.sh, then try again.");
                }
                if (!page.signedIn) {
                    return i18n("Sign in on the Account page.");
                }
                return i18n("Choose an empty folder on the Account page. Your OneDrive appears in it; files download when you open them.");
            }
            leading: Kirigami.Icon {
                source: page.status ? page.status.iconName : "state-offline"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.FormTextDelegate {
            visible: page.hasFolder && page.status !== null && page.status.attention.length > 0
            text: i18n("Needs your attention")
            description: page.status ? page.status.attention : ""
            leading: Kirigami.Icon {
                source: "dialog-warning"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.FormButtonDelegate {
            visible: page.hasFolder && page.sync !== null && page.sync.conflictCount > 0
            text: page.sync ? i18np("See the conflict", "See the %1 conflicts", page.sync.conflictCount) : ""
            icon.name: "document-duplicate"
            onClicked: page.window.showPage("conflicts")
        }
        FormCard.FormButtonDelegate {
            visible: page.account !== null && !page.available
            text: i18nc("@action:button", "Try Again")
            icon.name: "view-refresh"
            // M8: the daemon that came back owns every object — all re-fetch.
            onClicked: Accounts.retry()
        }
        FormCard.FormButtonDelegate {
            visible: page.available && !page.signedIn
            text: i18nc("@action:button", "Sign In…")
            icon.name: "go-next"
            onClicked: page.window.showPage("account")
        }
        FormCard.FormButtonDelegate {
            visible: page.signedIn && !page.hasFolder
            text: i18nc("@action:button", "Choose Folder…")
            icon.name: "folder-open"
            onClicked: page.window.showPage("account")
        }
    }

    // What the folder takes on this computer.
    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.hasFolder

        FormCard.FormTextDelegate {
            visible: page.sync !== null && page.sync.pinnedCount > 0
            text: page.sync ? i18np("Always on this device: %1 item", "Always on this device: %1 items", page.sync.pinnedCount) : ""
        }
        FormCard.FormTextDelegate {
            text: i18n("On this computer: %1", Qt.locale().formattedDataSize(page.sync ? page.sync.localBytes : 0))
            // Without a connected helper, nothing fills a placeholder on
            // open, so freeing a file up would leave it reading as zeros
            // until it is explicitly hydrated again — Free Up Space stays
            // off there.
            description: page.freeUpUnsafe
                ? i18n("This folder has no helper connected: a freed file would read as empty until downloaded again with Dolphin or konedrivectl sync hydrate, so Free Up Space is off here.")
                : i18n("Files you have opened stay downloaded until you free up their space.")
            trailing: QQC2.Button {
                objectName: "freeUpButton"
                text: i18nc("@action:button", "Free Up Space…")
                icon.name: "edit-clear"
                enabled: page.sync !== null && !page.sync.freeingUp && !page.freeUpUnsafe
                onClicked: freeUpDialog.open()
            }
        }
        FormCard.AbstractFormDelegate {
            objectName: "freeingUpRow"
            visible: page.sync !== null && page.sync.freeingUp
            background: null
            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing
                QQC2.BusyIndicator {
                    running: page.sync !== null && page.sync.freeingUp
                    implicitWidth: Kirigami.Units.iconSizes.medium
                    implicitHeight: Kirigami.Units.iconSizes.medium
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    text: i18n("Freeing up space…")
                }
            }
        }
        Kirigami.InlineMessage {
            id: freeUpMessage
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            // Not bound: the close button hides it, and the next result shows it again.
            visible: page.sync !== null && page.sync.freeUpResult.length > 0
            type: Kirigami.MessageType.Information
            text: page.sync ? page.sync.freeUpResult : ""
            showCloseButton: true

            Connections {
                target: page.sync
                function onFreeUpResultChanged() {
                    freeUpMessage.visible = page.sync.freeUpResult.length > 0;
                }
            }
            // Another account's result is its own.
            Connections {
                target: Current
                function onChanged() {
                    freeUpMessage.visible = page.sync !== null && page.sync.freeUpResult.length > 0;
                }
            }
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.hasFolder

        FormCard.FormButtonDelegate {
            visible: page.sync !== null && page.sync.rootSource === "onedrive"
            text: i18nc("@action:button", "Refresh Now")
            icon.name: "view-refresh"
            onClicked: page.sync.refresh()
        }
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Open in File Manager")
            icon.name: "system-file-manager"
            onClicked: page.sync.openFolder()
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.hasFolder

        FormCard.FormTextDelegate {
            visible: page.sync !== null && page.sync.rootSource === "onedrive"
            text: i18n("Read-only for now")
            description: i18n("Files in this folder cannot be changed yet, and nothing is sent to OneDrive.")
        }
        FormCard.FormTextDelegate {
            visible: page.sync !== null && page.sync.rootState === "no-interception"
            text: i18n("Files download only when you ask")
            description: i18n("%1 items. Download them from Dolphin, or with konedrivectl sync hydrate.", page.sync ? page.sync.itemsPlaced : 0)
        }
        FormCard.FormButtonDelegate {
            visible: page.sync !== null && page.sync.rootSource === "onedrive" && page.sync.skippedCount > 0
            text: page.sync ? i18np("%1 item is not in the folder", "%1 items are not in the folder", page.sync.skippedCount) : ""
            icon.name: "view-hidden"
            onClicked: page.window.showPage("skipped")
        }
    }

    Kirigami.PromptDialog {
        id: freeUpDialog
        objectName: "freeUpDialog"

        /// The account it was opened for (an AccountItem); null once that account is gone.
        property QtObject item: null

        function confirm() {
            if (item) {
                item.sync.freeUpSpace();
            }
            close();
        }

        onAboutToShow: item = Current.item
        title: i18nc("@title:window", "Free Up Space?")
        // Without a connected helper this promise is false (nothing fills a
        // placeholder on open), which is why the button that opens this
        // dialog is off in that case — the honest text stays here too.
        subtitle: page.freeUpUnsafe
            ? i18n("This folder has no helper connected: a freed file would read as empty until you download it again with Dolphin or konedrivectl sync hydrate, not automatically when you open it.")
            : i18n("Every downloaded file in the folder that is not open right now becomes online-only again. Nothing is deleted from OneDrive; a file downloads again when you open it.")
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: i18nc("@action:button", "Free Up Space")
                icon.name: "edit-clear"
                enabled: freeUpDialog.item !== null
                onTriggered: freeUpDialog.confirm()
            },
            Kirigami.Action {
                text: i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: freeUpDialog.close()
            }
        ]

        // Another account shown, or this one gone: not the question asked.
        Connections {
            target: Current
            function onChanged() {
                freeUpDialog.close();
            }
        }
    }
}
