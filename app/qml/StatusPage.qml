import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// What the OneDrive chosen in the sidebar is doing now, the page the window opens on: at the
/// top, what its folder is doing or what stops it, with the one thing to do (AccountStatus);
/// one line of what Issues lists; what moves now and what waits to upload; and the folder,
/// with Sync Now and Pause. An account with no folder has only the last step of adding it
/// here: its folder.
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
    /// The folder's syncing stopped on an error (moved, deleted, its disk gone): Try Again in
    /// the head, or another folder.
    readonly property bool stopped: hasFolder && sync.overallReason === "stopped"
    /// The folder syncs on: Sync Now and Pause have something to do.
    readonly property bool running: hasFolder && !stopped && !sync.paused
    /// What waits and goes up by itself, by why (the "waiting" rows of NotUploadedSummary()).
    readonly property var waitingRows: sync ? sync.notUploadedSummary.filter(row => row.group === "waiting") : []
    readonly property bool downloading: hasFolder && (sync.transfers.count > 0 || sync.downloadLeftCount > 0)
    readonly property bool uploading: hasFolder && (sync.uploads.count > 0 || sync.uploadLeftCount > 0)
    /// Something moves or waits: Now shows only then.
    readonly property bool moving: hasFolder && (status.scanLine.length > 0 || downloading || uploading || waitingRows.length > 0)
    readonly property var window: QQC2.ApplicationWindow.window

    /// A queue's time left: "about 12 min".
    function timeLeftText(seconds) {
        const minutes = Math.ceil(seconds / 60);
        if (seconds < 60) {
            return i18nc("@info time left, seconds", "about %1 s", seconds);
        }
        if (minutes < 60) {
            return i18nc("@info time left, minutes", "about %1 min", minutes);
        }
        if (seconds < 86400) {
            return minutes % 60 === 0 ? i18nc("@info time left, hours", "about %1 h", Math.floor(minutes / 60))
                                      : i18nc("@info time left, hours and minutes", "about %1 h %2 min", Math.floor(minutes / 60), minutes % 60);
        }
        const hours = Math.ceil(seconds / 3600);
        return hours % 24 === 0 ? i18nc("@info time left, days", "about %1 d", Math.floor(hours / 24))
                                : i18nc("@info time left, days and hours", "about %1 d %2 h", Math.floor(hours / 24), hours % 24);
    }

    /// What is left one way (issue #16): "12 files left · 400 MB · about 3 min".
    function leftText(left, bytes, seconds) {
        const parts = [left];
        if (bytes > 0) {
            parts.push(Qt.locale().formattedDataSize(bytes));
        }
        if (seconds > 0) {
            parts.push(timeLeftText(seconds));
        }
        return parts.join(" · ");
    }

    /// The counts the summary of what waits was last asked for at.
    property string shownCounts: ""

    /// The counts that move what waits: a full OneDrive and the files too big for it change
    /// reasons, not the pending count.
    function countsNow() {
        return sync ? sync.pendingCount + "/" + sync.blockedCount + "/" + sync.quotaFull + "/" + sync.spaceWaitingCount + "/" + sync.tooBigCount : "";
    }

    /// What waits has no signal of its own: its summary is read when the page is shown,
    /// shows another account, or a count moves while it is.
    function loadWaiting() {
        // hasFolder may not have seen the account go yet.
        if (visible && hasFolder && sync) {
            shownCounts = countsNow();
            sync.loadNotUploaded();
        }
    }

    objectName: "statusPage"
    title: window ? window.accountTitle(i18nc("@title", "Status")) : i18nc("@title", "Status")

    onVisibleChanged: loadWaiting()
    onSyncChanged: loadWaiting()
    onHasFolderChanged: loadWaiting()
    Connections {
        target: page.sync
        enabled: page.visible
        function onSyncChanged() {
            if (page.countsNow() !== page.shownCounts) {
                page.loadWaiting();
            }
        }
        function onServiceAvailableChanged() {
            page.loadWaiting();
        }
    }

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
                    // What is wrong and is no list on Issues: a sentence of its own.
                    QQC2.Label {
                        objectName: "troubleLine"
                        Layout.fillWidth: true
                        visible: text.length > 0
                        text: page.status ? page.status.troubleLine : ""
                        wrapMode: Text.Wrap
                        color: Kirigami.Theme.neutralTextColor
                    }
                    QQC2.Label {
                        objectName: "notUpdatedLine"
                        Layout.fillWidth: true
                        visible: page.hasFolder && text.length > 0
                        text: page.sync ? page.sync.notUpdated : ""
                        wrapMode: Text.Wrap
                        color: Kirigami.Theme.neutralTextColor
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
                    }
                }
            }
        }
    }

    // What Issues lists that needs the user, in one line that leads there.
    FormCard.FormCard {
        objectName: "issuesCard"
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.hasFolder && page.status.issuesLine.length > 0

        FormCard.FormButtonDelegate {
            objectName: "issuesLine"
            text: page.status ? page.status.issuesLine : ""
            description: i18n("Issues says why, and what to do.")
            icon.name: "dialog-warning"
            onClicked: page.window.showPage("issues")
        }
    }

    // What moves now and what waits to upload: whether it all got there.
    FormCard.FormHeader {
        visible: page.moving
        title: i18nc("@title:group", "Now")
    }
    FormCard.FormCard {
        objectName: "nowCard"
        visible: page.moving

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
        FormCard.FormTextDelegate {
            objectName: "downloadLine"
            visible: page.downloading
            text: page.sync && page.sync.activeDownloads > 0
                  ? i18ncp("@info files downloading now, speed", "Downloading 1 file at %2/s", "Downloading %1 files at %2/s",
                           page.sync.activeDownloads, Qt.locale().formattedDataSize(page.sync.downloadSpeed))
                  : i18n("Downloading")
            description: page.sync && page.sync.downloadLeftCount > 0
                         ? page.leftText(i18ncp("@info files left to download", "%1 file left", "%1 files left", page.sync.downloadLeftCount),
                                         page.sync.downloadLeftBytes, page.sync.downloadTimeLeft)
                         : ""
            leading: Kirigami.Icon {
                source: "cloud-download"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        Repeater {
            model: page.sync ? page.sync.transfers : null
            delegate: TransferRow {}
        }
        FormCard.FormTextDelegate {
            objectName: "uploadLine"
            visible: page.uploading
            text: page.sync && page.sync.activeUploads > 0
                  ? i18ncp("@info files uploading now, speed", "Uploading 1 file at %2/s", "Uploading %1 files at %2/s",
                           page.sync.activeUploads, Qt.locale().formattedDataSize(page.sync.uploadSpeed))
                  : i18n("Uploading")
            description: page.sync && page.sync.uploadLeftCount > 0
                         ? page.leftText(i18ncp("@info changes left to upload", "%1 change left", "%1 changes left", page.sync.uploadLeftCount),
                                         page.sync.uploadLeftBytes, page.sync.uploadTimeLeft)
                         : ""
            leading: Kirigami.Icon {
                source: "cloud-upload"
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
        }
        Repeater {
            model: page.sync ? page.sync.uploads : null
            delegate: TransferRow {}
        }
        // Why changes wait; what can't upload is the line above, on Issues.
        Repeater {
            model: page.waitingRows
            delegate: FormCard.FormTextDelegate {
                required property var modelData
                objectName: "waitingRow"
                text: i18np("1 change: %2", "%1 changes: %2", modelData.count, modelData.why)
                description: i18n("Goes up by itself.")
                textItem.wrapMode: Text.Wrap
                leading: Kirigami.Icon {
                    source: "chronometer"
                    implicitWidth: Kirigami.Units.iconSizes.medium
                    implicitHeight: Kirigami.Units.iconSizes.medium
                }
            }
        }
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
                    objectName: "openFolderButton"
                    visible: !page.stopped
                    text: i18nc("@action:button", "Open Folder")
                    icon.name: "system-file-manager"
                    onClicked: page.sync.openFolder()
                }
                // Moved, deleted or on a disk that is gone: Try Again is in the head, and this.
                QQC2.Button {
                    objectName: "anotherFolderButton"
                    visible: page.stopped
                    text: i18nc("@action:button", "Use Another Folder…")
                    icon.name: "folder-open"
                    onClicked: anotherFolderDialog.open()
                }
                QQC2.Button {
                    objectName: "syncNowButton"
                    visible: page.running
                    text: i18nc("@action:button", "Sync Now")
                    icon.name: "view-refresh"
                    onClicked: page.sync.refresh()
                }
                // Paused, Resume is in the head.
                QQC2.Button {
                    id: pauseButton
                    objectName: "pauseButton"
                    visible: page.running
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
            }
        }
    }

    AnotherFolderDialog {
        id: anotherFolderDialog
        objectName: "anotherFolderDialog"
    }

    /// One file moving, with how far it is.
    component TransferRow: FormCard.AbstractFormDelegate {
        id: row

        required property string name
        required property var done
        required property var total
        required property real fraction

        Layout.fillWidth: true
        background: null
        contentItem: ColumnLayout {
            spacing: Kirigami.Units.smallSpacing
            QQC2.Label {
                Layout.fillWidth: true
                text: row.name
                elide: Text.ElideMiddle
            }
            QQC2.ProgressBar {
                Layout.fillWidth: true
                from: 0
                to: 1
                value: row.fraction
                indeterminate: row.total === 0
            }
            QQC2.Label {
                Layout.fillWidth: true
                text: i18n("%1 of %2", Qt.locale().formattedDataSize(row.done), Qt.locale().formattedDataSize(row.total))
                opacity: 0.7
            }
        }
    }
}
