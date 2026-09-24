import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The start page: how the folder is doing, and what to do about it. With no
/// service, no sign-in or no folder, it says so and leads to the page that
/// fixes it (Account or Settings).
FormCard.FormCardPage {
    id: page

    readonly property bool available: Account.serviceAvailable && Sync.serviceAvailable
    readonly property bool signedIn: available && Account.state === "signed-in"
    readonly property bool hasFolder: available && Sync.rootPath.length > 0
    // A no-interception folder is one Free Up Space warning; a folder that
    // shows OneDrive but whose helper is not connected (including a legacy
    // folder still waiting to switch, which publishes RootState "error", not
    // "no-interception") is the same warning by a different RootState.
    readonly property bool freeUpUnsafe: Sync.rootState === "no-interception" || Sync.helperState !== "connected"
    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "statusPage"
    title: i18nc("@title", "Status")

    // The helper: nothing keeps a folder in step, and nothing downloads on
    // open, while this is anything but "connected" (dbus/org.konedrive.Sync1.xml).
    FormCard.FormCard {
        objectName: "helperCard"
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: Sync.helperTrouble

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
            visible: Sync.helperInstruction.length > 0
            background: null
            contentItem: TextEdit {
                text: Sync.helperInstruction
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
            onClicked: Sync.retry()
        }
    }

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: Sync.actionError
        visible: text.length > 0
    }

    // The folder and the status line, or what stands in the way.
    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing

        FormCard.FormTextDelegate {
            text: page.hasFolder ? Sync.rootPath : Status.text
            description: {
                if (page.hasFolder) {
                    return Status.text;
                }
                if (!page.available) {
                    return i18n("Install it with scripts/dev-install.sh, then try again.");
                }
                if (!page.signedIn) {
                    return i18n("Sign in on the Account page.");
                }
                return i18n("Choose an empty folder in Settings. Your OneDrive appears in it; files download when you open them.");
            }
            leading: Kirigami.Icon {
                source: Status.iconName
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.FormTextDelegate {
            visible: page.hasFolder && Status.attention.length > 0
            text: i18n("Needs your attention")
            description: Status.attention
            leading: Kirigami.Icon {
                source: "dialog-warning"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.FormButtonDelegate {
            visible: page.hasFolder && Sync.conflictCount > 0
            text: i18np("See the conflict", "See the %1 conflicts", Sync.conflictCount)
            icon.name: "document-duplicate"
            onClicked: page.window.showPage("conflicts")
        }
        FormCard.FormButtonDelegate {
            visible: !page.available
            text: i18nc("@action:button", "Try Again")
            icon.name: "view-refresh"
            // M8: the daemon that came back also owns Sync1 — both re-fetch.
            onClicked: {
                Account.retry();
                Sync.retry();
            }
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
            onClicked: page.window.showPage("settings")
        }
    }

    // What the folder takes on this computer.
    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.hasFolder

        FormCard.FormTextDelegate {
            text: i18n("On this computer: %1", Qt.locale().formattedDataSize(Sync.localBytes))
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
                enabled: !Sync.freeingUp && !page.freeUpUnsafe
                onClicked: freeUpDialog.open()
            }
        }
        FormCard.AbstractFormDelegate {
            objectName: "freeingUpRow"
            visible: Sync.freeingUp
            background: null
            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing
                QQC2.BusyIndicator {
                    running: Sync.freeingUp
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
            visible: Sync.freeUpResult.length > 0
            type: Kirigami.MessageType.Information
            text: Sync.freeUpResult
            showCloseButton: true

            Connections {
                target: Sync
                function onFreeUpResultChanged() {
                    freeUpMessage.visible = Sync.freeUpResult.length > 0;
                }
            }
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.hasFolder

        FormCard.FormButtonDelegate {
            visible: Sync.rootSource === "onedrive"
            text: i18nc("@action:button", "Refresh Now")
            icon.name: "view-refresh"
            onClicked: Sync.refresh()
        }
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Open in File Manager")
            icon.name: "system-file-manager"
            onClicked: Sync.openFolder()
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.hasFolder

        FormCard.FormTextDelegate {
            visible: Sync.rootSource === "onedrive"
            text: i18n("Read-only for now")
            description: i18n("Files in this folder cannot be changed yet, and nothing is sent to OneDrive.")
        }
        FormCard.FormTextDelegate {
            visible: Sync.rootState === "no-interception"
            text: i18n("Files download only when you ask")
            description: i18n("%1 items. Download them from Dolphin, or with konedrivectl sync hydrate.", Sync.itemsPlaced)
        }
        FormCard.FormButtonDelegate {
            visible: Sync.rootSource === "onedrive" && Sync.skippedCount > 0
            text: i18np("%1 item is not in the folder", "%1 items are not in the folder", Sync.skippedCount)
            icon.name: "view-hidden"
            onClicked: page.window.showPage("skipped")
        }
    }

    Kirigami.PromptDialog {
        id: freeUpDialog
        objectName: "freeUpDialog"
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
                onTriggered: {
                    Sync.freeUpSpace();
                    freeUpDialog.close();
                }
            },
            Kirigami.Action {
                text: i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: freeUpDialog.close()
            }
        ]
    }
}
