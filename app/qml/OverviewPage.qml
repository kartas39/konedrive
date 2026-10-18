import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The start page, for the account chosen in the sidebar: at the top, what its folder is
/// doing or what stops it, with the one thing to do (AccountStatus); then how it syncs, the
/// folder with Pause and Sync Now, what needs the user, what moves now, and the storage.
/// An account with no folder has only the last step of adding it here: its folder.
FormCard.FormCardPage {
    id: page

    readonly property var account: Current.account
    readonly property var sync: Current.sync
    readonly property var status: Current.status
    // Current.account and Current.sync change together, but these bindings
    // may see one before the other: each checks both.
    readonly property bool available: Daemon.serviceAvailable && account !== null && sync !== null && status !== null && account.serviceAvailable
                                      && sync.serviceAvailable
    readonly property bool hasFolder: available && sync.rootPath.length > 0
    /// Signed in with no folder: the folder is chosen here, and nothing else shows.
    readonly property bool choosingFolder: available && status.action === AccountStatus.ChooseFolder
    /// Something under Needs Your Attention has a line: from the counts, since a line
    /// inside a hidden card reads as hidden itself.
    readonly property bool needsAttention: hasFolder && (sync.quotaFull || sync.tooBigCount > 0 || sync.blockedCount > 0 || sync.conflictCount > 0
                                                         || sync.notUpdated.length > 0 || status.troubleLine.length > 0)
    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "overviewPage"
    title: window ? window.accountTitle(i18nc("@title", "Overview")) : i18nc("@title", "Overview")

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
    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: page.sync ? page.sync.actionError : ""
        visible: page.available && text.length > 0
    }
    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: page.account ? page.account.actionError : ""
        visible: page.available && text.length > 0
    }

    ServiceCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: !Daemon.serviceAvailable
    }

    FolderChoiceCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.choosingFolder
    }

    // The head: what the folder is doing or what stops it, and the one thing to do.
    FormCard.FormCard {
        objectName: "headCard"
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.available && !page.choosingFolder

        FormCard.AbstractFormDelegate {
            background: null
            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing

                Kirigami.Icon {
                    Layout.alignment: Qt.AlignTop
                    source: page.status ? page.status.iconName : "state-offline"
                    implicitWidth: Kirigami.Units.iconSizes.large
                    implicitHeight: Kirigami.Units.iconSizes.large
                }
                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: Kirigami.Units.smallSpacing

                    Kirigami.Heading {
                        objectName: "headline"
                        Layout.fillWidth: true
                        level: 2
                        text: page.status ? page.status.headline : ""
                        wrapMode: Text.Wrap
                    }
                    QQC2.Label {
                        objectName: "headDetail"
                        Layout.fillWidth: true
                        visible: text.length > 0
                        text: page.status ? page.status.detail : ""
                        wrapMode: Text.Wrap
                        opacity: 0.8
                    }
                    Flow {
                        objectName: "headActions"
                        Layout.fillWidth: true
                        Layout.topMargin: Kirigami.Units.smallSpacing
                        spacing: Kirigami.Units.smallSpacing
                        visible: page.status !== null && page.status.action !== AccountStatus.None

                        readonly property int action: page.status ? page.status.action : AccountStatus.None

                        QQC2.Button {
                            visible: parent.action === AccountStatus.CancelSignIn
                            text: i18nc("@action:button", "Copy Sign-In Link")
                            icon.name: "edit-copy"
                            enabled: page.account !== null && page.account.signInUrl.length > 0
                            onClicked: page.account.copySignInUrl()
                        }
                        QQC2.Button {
                            visible: parent.action === AccountStatus.CancelSignIn
                            text: i18nc("@action:button", "Cancel")
                            icon.name: "dialog-cancel"
                            onClicked: page.account.cancelSignIn()
                        }
                        QQC2.Button {
                            visible: parent.action === AccountStatus.SignIn
                            text: i18nc("@action:button", "Sign In Again")
                            icon.name: "go-next"
                            onClicked: page.account.signIn()
                        }
                        QQC2.Button {
                            objectName: "tryAgainButton"
                            visible: parent.action === AccountStatus.TryAgain
                            text: i18nc("@action:button", "Try Again")
                            icon.name: "view-refresh"
                            onClicked: page.sync.refresh()
                        }
                        QQC2.Button {
                            objectName: "resumeButton"
                            visible: parent.action === AccountStatus.Resume
                            text: i18nc("@action:button", "Resume")
                            icon.name: "media-playback-start"
                            onClicked: page.sync.resume()
                        }
                        QQC2.Button {
                            objectName: "syncAnywayButton"
                            visible: parent.action === AccountStatus.SyncAnyway
                            text: i18nc("@action:button", "Sync Anyway")
                            icon.name: "media-playback-start"
                            onClicked: page.sync.syncAnyway()
                        }
                        QQC2.Button {
                            objectName: "reviewButton"
                            visible: parent.action === AccountStatus.Review
                            text: i18nc("@action:button", "Review")
                            icon.name: "dialog-warning"
                            onClicked: page.window.showPage("issues")
                        }
                    }
                }
            }
        }
    }

    // How the folder syncs.
    ModeCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.hasFolder
    }

    // The folder, and what the user does with it.
    FormCard.FormCard {
        objectName: "folderCard"
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.hasFolder

        FormCard.FormTextDelegate {
            text: page.sync ? page.sync.rootPathText : ""
            description: i18n("Your OneDrive on this device")
            leading: Kirigami.Icon {
                source: "folder-cloud"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.AbstractFormDelegate {
            background: null
            contentItem: Flow {
                spacing: Kirigami.Units.smallSpacing

                QQC2.Button {
                    text: i18nc("@action:button", "Open Folder")
                    icon.name: "system-file-manager"
                    onClicked: page.sync.openFolder()
                }
                QQC2.Button {
                    objectName: "openOnWebButton"
                    text: i18nc("@action:button", "Open on the Web")
                    icon.name: "internet-services"
                    onClicked: page.sync.openOnWeb()
                }
                QQC2.Button {
                    objectName: "syncNowButton"
                    text: i18nc("@action:button", "Sync Now")
                    icon.name: "view-refresh"
                    onClicked: page.sync.refresh()
                }
                QQC2.Button {
                    id: pauseButton
                    objectName: "pauseButton"
                    visible: page.sync !== null && !page.sync.paused
                    text: i18nc("@action:button", "Pause")
                    icon.name: "media-playback-pause"
                    onClicked: pauseMenu.popup(pauseButton, 0, pauseButton.height)

                    QQC2.Menu {
                        id: pauseMenu
                        objectName: "pauseMenu"

                        QQC2.MenuItem {
                            text: i18nc("@action:inmenu pause syncing", "For 2 Hours")
                            onTriggered: page.sync.pause(2 * 3600)
                        }
                        QQC2.MenuItem {
                            text: i18nc("@action:inmenu pause syncing", "For 8 Hours")
                            onTriggered: page.sync.pause(8 * 3600)
                        }
                        QQC2.MenuItem {
                            text: i18nc("@action:inmenu pause syncing", "For 24 Hours")
                            onTriggered: page.sync.pause(24 * 3600)
                        }
                        QQC2.MenuItem {
                            text: i18nc("@action:inmenu pause syncing", "Until Resumed")
                            onTriggered: page.sync.pause(0)
                        }
                    }
                }
                QQC2.Button {
                    objectName: "resumeControl"
                    visible: page.sync !== null && page.sync.paused
                    text: i18nc("@action:button", "Resume")
                    icon.name: "media-playback-start"
                    onClicked: page.sync.resume()
                }
            }
        }
        FormCard.FormButtonDelegate {
            objectName: "skippedLine"
            visible: page.sync !== null && page.sync.skippedCount > 0
            text: page.sync ? i18np("1 item in OneDrive can't be shown in the folder", "%1 items in OneDrive can't be shown in the folder", page.sync.skippedCount) : ""
            icon.name: "view-hidden"
            onClicked: page.window.showPage("skipped")
        }
    }

    // What needs the user, each with what to do.
    FormCard.FormHeader {
        visible: page.needsAttention
        title: i18nc("@title:group", "Needs Your Attention")
    }
    FormCard.FormCard {
        objectName: "attentionCard"
        visible: page.needsAttention

        // A full OneDrive (issue #2): every change that waits for space, in one line.
        FormCard.FormTextDelegate {
            id: quotaFullLine
            objectName: "quotaFullLine"
            visible: page.sync !== null && page.sync.quotaFull
            text: page.sync ? i18np("OneDrive is full: 1 file (%2) waits for space",
                                    "OneDrive is full: %1 files (%2) wait for space",
                                    page.sync.spaceWaitingCount,
                                    Qt.locale().formattedDataSize(page.sync.spaceWaitingBytes)) : ""
            description: i18n("Free up space in OneDrive, then check again.")
            textItem.wrapMode: Text.Wrap
            leading: Kirigami.Icon {
                source: "dialog-warning"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.AbstractFormDelegate {
            visible: quotaFullLine.visible
            background: null
            contentItem: Flow {
                spacing: Kirigami.Units.smallSpacing
                QQC2.Button {
                    text: i18nc("@action:button", "Open OneDrive on the Web")
                    icon.name: "internet-services"
                    onClicked: page.sync.openOnWeb()
                }
                QQC2.Button {
                    objectName: "checkAgainButton"
                    text: i18nc("@action:button", "Check Again")
                    icon.name: "view-refresh"
                    onClicked: page.sync.refresh()
                }
            }
        }
        FormCard.FormButtonDelegate {
            id: tooBigLine
            objectName: "tooBigLine"
            visible: page.sync !== null && page.sync.tooBigCount > 0
            text: page.sync ? i18np("1 file is too big for the space left in OneDrive", "%1 files are too big for the space left in OneDrive", page.sync.tooBigCount) : ""
            icon.name: "dialog-warning"
            onClicked: page.window.showPage("issues")
        }
        FormCard.FormButtonDelegate {
            id: blockedLine
            objectName: "blockedLine"
            visible: page.sync !== null && page.sync.blockedCount > 0
            text: page.sync ? i18np("1 change can't upload", "%1 changes can't upload", page.sync.blockedCount) : ""
            description: i18n("Issues says why, and what to do.")
            icon.name: "dialog-warning"
            onClicked: page.window.showPage("issues")
        }
        FormCard.FormButtonDelegate {
            id: conflictsLine
            objectName: "conflictsLine"
            visible: page.sync !== null && page.sync.conflictCount > 0
            text: page.sync ? i18np("Kept both versions of 1 file changed here and in OneDrive", "Kept both versions of %1 files changed here and in OneDrive", page.sync.conflictCount) : ""
            icon.name: "document-duplicate"
            onClicked: page.window.showPage("conflicts")
        }
        FormCard.FormTextDelegate {
            id: notUpdatedLine
            visible: page.sync !== null && page.sync.notUpdated.length > 0
            text: page.sync ? page.sync.notUpdated : ""
            textItem.wrapMode: Text.Wrap
            leading: Kirigami.Icon {
                source: "dialog-warning"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.FormTextDelegate {
            id: troubleLine
            objectName: "troubleLine"
            visible: page.status !== null && page.status.troubleLine.length > 0
            text: page.status ? page.status.troubleLine : ""
            textItem.wrapMode: Text.Wrap
            leading: Kirigami.Icon {
                source: "dialog-warning"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
    }

    // What moves now; the rest is on Activity.
    FormCard.FormHeader {
        visible: page.hasFolder
        title: i18nc("@title:group", "Now")
    }
    FormCard.FormCard {
        objectName: "nowCard"
        visible: page.hasFolder

        // The check of the local files at the start of the sync, while it runs.
        FormCard.FormTextDelegate {
            objectName: "scanLine"
            visible: page.status !== null && page.status.scanLine.length > 0
            text: page.status ? page.status.scanLine : ""
            leading: Kirigami.Icon {
                source: "folder-sync"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        FormCard.FormButtonDelegate {
            objectName: "nowLine"
            text: {
                if (!page.sync) {
                    return "";
                }
                const parts = [];
                if (page.sync.activeDownloads > 0) {
                    parts.push(i18ncp("@info files downloading now, speed", "Downloading 1 file at %2/s", "Downloading %1 files at %2/s",
                                      page.sync.activeDownloads, Qt.locale().formattedDataSize(page.sync.downloadSpeed)));
                }
                if (page.sync.activeUploads > 0) {
                    parts.push(i18ncp("@info files uploading now, speed", "uploading 1 file at %2/s", "uploading %1 files at %2/s",
                                      page.sync.activeUploads, Qt.locale().formattedDataSize(page.sync.uploadSpeed)));
                }
                if (parts.length === 0) {
                    return page.sync.pendingCount > 0 ? i18np("1 change will upload shortly", "%1 changes will upload shortly", page.sync.pendingCount)
                                                      : i18n("Nothing is moving");
                }
                const line = parts.join(", ");
                return line.charAt(0).toUpperCase() + line.slice(1);
            }
            description: i18n("Activity shows what moves and what happened.")
            icon.name: "view-history"
            onClicked: page.window.showPage("activity")
        }
    }

    // How much is used, in OneDrive and on this device.
    FormCard.FormHeader {
        visible: page.hasFolder
        title: i18nc("@title:group", "Storage")
    }
    FormCard.FormCard {
        objectName: "storageCard"
        visible: page.hasFolder

        FormCard.AbstractFormDelegate {
            visible: page.account !== null && page.account.quotaTotal > 0
            background: null
            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing

                QQC2.Label {
                    Layout.fillWidth: true
                    text: page.account ? i18n("OneDrive: %1 of %2 used",
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
        FormCard.FormDelegateSeparator {
            visible: page.account !== null && page.account.quotaTotal > 0
        }
        FormCard.FormTextDelegate {
            text: i18n("On this device: %1", Qt.locale().formattedDataSize(page.sync ? page.sync.localBytes : 0))
            description: page.sync && page.sync.pinnedCount > 0
                         ? i18np("Files you open stay downloaded until you free up their space. 1 item is always kept on this device.",
                                 "Files you open stay downloaded until you free up their space. %1 items are always kept on this device.",
                                 page.sync.pinnedCount)
                         : i18n("Files you open stay downloaded until you free up their space.")
            textItem.wrapMode: Text.Wrap
            trailing: QQC2.Button {
                objectName: "freeUpButton"
                text: i18nc("@action:button", "Free Up Space…")
                icon.name: "edit-clear"
                enabled: page.sync !== null && !page.sync.freeingUp
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
        subtitle: i18n("Every downloaded file in the folder that is not open right now becomes online-only again, except what is always kept on this device and what waits to upload. Nothing is deleted from OneDrive; a file downloads again when you open it.")
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
