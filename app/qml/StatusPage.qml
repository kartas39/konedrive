import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.kde.kitemmodels as KItemModels
import org.konedrive.app

/// The OneDrive chosen in the sidebar on one page, the one the window opens on. In its header
/// Sync Now, Pause (Resume while paused) and Open Folder. Then what its folder is doing or
/// what stops it, with the one thing to do, and one line of what Issues lists; the space it
/// takes, with Free Up Space and Unpin All; each way's speed over the last two minutes and
/// what is left, the transfer pool, and the files moving now; and its History, folded until
/// it is opened, with a search. An account with no folder yet has Choose Folder… in its head,
/// and one whose syncing stopped only why, with Try Again and Use Another Folder….
///
/// One list: what is above History is its header, and History's events are its rows, built
/// only while it is open and only those in sight.
ListPage {
    id: page

    readonly property var account: Current.account
    readonly property var sync: Current.sync
    readonly property var status: Current.status
    // Current.account and Current.sync change together, but these bindings
    // may see one before the other: each checks both.
    readonly property bool available: Daemon.serviceAvailable && account !== null && sync !== null && status !== null && account.serviceAvailable
                                      && sync.serviceAvailable
    readonly property bool hasFolder: available && sync.rootPath.length > 0
    /// The folder's syncing stopped on an error (moved, deleted, its disk gone).
    readonly property bool stopped: hasFolder && sync.overallReason === "stopped"
    /// The folder syncs: what it holds, moves and did shows.
    readonly property bool working: hasFolder && !stopped
    /// It syncs on: Sync Now and Pause have something to do.
    readonly property bool running: working && !sync.paused
    /// What waits and goes up by itself, by why (the "waiting" rows of NotUploadedSummary()).
    readonly property var waitingRows: sync ? sync.notUploadedSummary.filter(row => row.group === "waiting") : []
    /// Uploads show unless the account is read-only and nothing goes up any more.
    readonly property bool uploadsShown: working && account !== null
                                         && (account.mode === "read-write" || sync.uploads.count > 0 || sync.uploadLeftCount > 0)
    /// History is open: its events are the list's rows.
    property bool historyShown: false
    /// What History's search keeps: events whose path holds it, in any case.
    property string searchText: ""
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
        // working may not have seen the account go yet.
        if (visible && working && sync) {
            shownCounts = countsNow();
            sync.loadNotUploaded();
        }
    }

    /// History opened and brought into sight: where a notice about a file leads.
    function showHistory() {
        historyShown = true;
        Qt.callLater(() => {
            const head = list.headerItem;
            if (!head) {
                return;
            }
            const wanted = head.y + head.historyY;
            const last = list.originY + list.contentHeight - list.height;
            list.contentY = Math.max(list.originY, Math.min(wanted, last));
        });
    }

    objectName: "statusPage"
    title: window ? window.accountTitle(i18nc("@title", "Status")) : i18nc("@title", "Status")

    onVisibleChanged: loadWaiting()
    onSyncChanged: {
        // A search is in this account's events.
        searchText = "";
        loadWaiting();
    }
    onWorkingChanged: loadWaiting()
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

    // In the page's header, beside its title.
    actions: [
        Kirigami.Action {
            objectName: "syncNowAction"
            visible: page.running
            text: i18nc("@action:button", "Sync Now")
            icon.name: "view-refresh"
            displayHint: Kirigami.DisplayHint.IconOnly
            onTriggered: page.sync.refresh()
        },
        Kirigami.Action {
            objectName: "pauseAction"
            visible: page.running
            text: i18nc("@action:button", "Pause")
            icon.name: "media-playback-pause"
            displayHint: Kirigami.DisplayHint.IconOnly

            Kirigami.Action {
                text: i18nc("@action:inmenu pause syncing", "For 2 Hours")
                onTriggered: page.sync.pause(2 * 3600)
            }
            Kirigami.Action {
                text: i18nc("@action:inmenu pause syncing", "For 8 Hours")
                onTriggered: page.sync.pause(8 * 3600)
            }
            Kirigami.Action {
                text: i18nc("@action:inmenu pause syncing", "For 24 Hours")
                onTriggered: page.sync.pause(24 * 3600)
            }
            Kirigami.Action {
                text: i18nc("@action:inmenu pause syncing", "Until Resumed")
                onTriggered: page.sync.pause(0)
            }
        },
        Kirigami.Action {
            objectName: "resumeAction"
            visible: page.working && page.sync.paused
            text: i18nc("@action:button", "Resume")
            icon.name: "media-playback-start"
            displayHint: Kirigami.DisplayHint.IconOnly
            onTriggered: page.sync.resume()
        },
        Kirigami.Action {
            objectName: "openFolderAction"
            visible: page.working
            text: i18nc("@action:button", "Open Folder")
            tooltip: page.sync ? i18nc("@info:tooltip the folder's path", "Open %1", page.sync.rootPathText) : ""
            icon.name: "system-file-manager"
            displayHint: Kirigami.DisplayHint.IconOnly
            onTriggered: page.sync.openFolder()
        }
    ]

    ListView {
        id: list

        /// History's events, those the search keeps.
        readonly property KItemModels.KSortFilterProxyModel events: KItemModels.KSortFilterProxyModel {
            sourceModel: page.sync ? page.sync.activity : null
            filterRoleName: "path"
            filterString: page.searchText
            filterCaseSensitivity: Qt.CaseInsensitive
        }

        objectName: "historyList"
        reuseItems: true
        model: page.historyShown && page.working ? events : null

        header: ColumnLayout {
            /// Where History begins, in the header: showHistory() brings it into sight.
            readonly property real historyY: historyCard.y

            width: ListView.view.width
            spacing: 0

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
                Layout.fillWidth: true
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: !Daemon.serviceAvailable
            }

            // Why the folder picked for the account cannot be its folder.
            Kirigami.InlineMessage {
                objectName: "folderRefusal"
                Layout.fillWidth: true
                Layout.topMargin: Kirigami.Units.largeSpacing
                Layout.leftMargin: Kirigami.Units.largeSpacing
                Layout.rightMargin: Kirigami.Units.largeSpacing
                type: Kirigami.MessageType.Warning
                text: FolderChoice.refusal
                visible: page.available && text.length > 0
            }

            StoppedFolderCard {
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.stopped
                onChooseFolderRequested: page.window.chooseFolder(Current.item)
            }

            // The head: what the folder is doing or what stops it, the one thing to do, the
            // check of the local files while it runs, and what Issues lists.
            FormCard.FormCard {
                objectName: "headCard"
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.available && !page.stopped

                FormCard.AbstractFormDelegate {
                    background: null
                    contentItem: RowLayout {
                        spacing: Kirigami.Units.largeSpacing

                        Kirigami.Icon {
                            Layout.alignment: Qt.AlignTop
                            source: page.status ? page.status.iconName : "state-offline"
                            implicitWidth: Kirigami.Units.iconSizes.medium
                            implicitHeight: Kirigami.Units.iconSizes.medium
                        }
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: Kirigami.Units.smallSpacing

                            Kirigami.Heading {
                                objectName: "headline"
                                Layout.fillWidth: true
                                level: 3
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
                            // The check of the local files at the start of the sync, while it runs.
                            QQC2.Label {
                                objectName: "scanLine"
                                Layout.fillWidth: true
                                visible: page.working && text.length > 0
                                text: page.status ? page.status.scanLine : ""
                                wrapMode: Text.Wrap
                                opacity: 0.8
                            }
                            // Resume and Try Again are elsewhere: in the page's header and on the
                            // card of a folder that stopped.
                            Flow {
                                objectName: "headActions"
                                Layout.fillWidth: true
                                Layout.topMargin: Kirigami.Units.smallSpacing
                                spacing: Kirigami.Units.smallSpacing
                                visible: action === AccountStatus.CancelSignIn || action === AccountStatus.SignIn || action === AccountStatus.SyncAnyway
                                         || action === AccountStatus.ChooseFolder

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
                                    objectName: "chooseFolderButton"
                                    visible: parent.action === AccountStatus.ChooseFolder
                                    text: i18nc("@action:button", "Choose Folder…")
                                    icon.name: "folder-open"
                                    onClicked: page.window.chooseFolder(Current.item)
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
                // What Issues lists that needs the user, in one line that leads there.
                FormCard.FormDelegateSeparator {
                    visible: issuesLine.visible
                }
                FormCard.FormButtonDelegate {
                    id: issuesLine
                    objectName: "issuesLine"
                    visible: page.working && page.status.issuesLine.length > 0
                    text: page.status ? page.status.issuesLine : ""
                    description: i18n("Issues says why, and what to do.")
                    icon.name: "dialog-warning"
                    onClicked: page.window.showPage("issues")
                }
            }

            // The space: on this device, used in OneDrive, free; Free Up Space and Unpin All.
            FormCard.FormCard {
                objectName: "storageCard"
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.working

                FormCard.AbstractFormDelegate {
                    background: null
                    contentItem: RowLayout {
                        spacing: Kirigami.Units.largeSpacing

                        StorageBar {
                            Layout.fillWidth: true
                            Layout.alignment: Qt.AlignVCenter
                            localBytes: page.sync ? page.sync.localBytes : 0
                            usedBytes: page.account ? page.account.quotaUsed : 0
                            totalBytes: page.account ? page.account.quotaTotal : 0
                            pinnedCount: page.sync ? page.sync.pinnedCount : 0
                            pinnedBytes: page.sync ? page.sync.pinnedBytes : 0
                        }
                        ColumnLayout {
                            Layout.alignment: Qt.AlignVCenter
                            spacing: Kirigami.Units.smallSpacing

                            QQC2.Button {
                                objectName: "freeUpButton"
                                Layout.fillWidth: true
                                visible: page.sync !== null && !page.sync.freeingUp
                                text: i18nc("@action:button", "Free Up Space…")
                                icon.name: "edit-clear"
                                onClicked: freeUpDialog.open()
                            }
                            RowLayout {
                                objectName: "freeingUpRow"
                                visible: page.sync !== null && page.sync.freeingUp
                                spacing: Kirigami.Units.smallSpacing
                                QQC2.BusyIndicator {
                                    running: parent.visible
                                    implicitWidth: Kirigami.Units.iconSizes.smallMedium
                                    implicitHeight: Kirigami.Units.iconSizes.smallMedium
                                }
                                QQC2.Label {
                                    text: i18n("Freeing up space…")
                                }
                            }
                            QQC2.Button {
                                objectName: "unpinAllButton"
                                Layout.fillWidth: true
                                visible: page.sync !== null && page.sync.pinnedCount > 0
                                text: i18nc("@action:button", "Unpin All…")
                                icon.name: "window-unpin"
                                onClicked: unpinAllDialog.open()
                            }
                        }
                    }
                }
                // What the last Free Up did, until it is closed: closing lets it go.
                Kirigami.InlineMessage {
                    objectName: "freeUpMessage"
                    Layout.fillWidth: true
                    Layout.margins: Kirigami.Units.smallSpacing
                    visible: page.sync !== null && page.sync.freeUpResult.length > 0
                    type: Kirigami.MessageType.Information
                    text: page.sync ? page.sync.freeUpResult : ""
                    actions: Kirigami.Action {
                        objectName: "closeFreeUpMessage"
                        text: i18nc("@action:button", "Close")
                        icon.name: "dialog-close"
                        displayHint: Kirigami.DisplayHint.IconOnly
                        onTriggered: page.sync.clearFreeUpResult()
                    }
                }
            }

            // Each way: its speed over the last two minutes, and what is left. Uploads go
            // while the account is read-only and nothing goes up: downloads take the width.
            RowLayout {
                objectName: "transferCards"
                Layout.fillWidth: true
                Layout.maximumWidth: Kirigami.Units.gridUnit * 30
                Layout.alignment: Qt.AlignHCenter
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.working
                spacing: Kirigami.Units.largeSpacing

                TransferCard {
                    id: downloading
                    objectName: "downloadingCard"
                    title: i18nc("@title a mini card", "Downloading")
                    speed: page.sync ? page.sync.downloadSpeed : 0
                    active: page.sync ? page.sync.activeDownloads : 0
                    speedHistory: page.sync ? page.sync.downloadSpeedHistory : []
                    activeHistory: page.sync ? page.sync.activeDownloadsHistory : []
                    filesText: i18ncp("@info files downloading at once", "%1 file downloading", "%1 files downloading", downloading.active)
                    filesLegend: i18nc("@info chart legend", "Files downloading")
                    leftCount: page.sync ? page.sync.downloadLeftCount : 0
                    leftBytes: page.sync ? page.sync.downloadLeftBytes : 0
                    timeText: page.sync && page.sync.downloadTimeLeft > 0 ? page.timeLeftText(page.sync.downloadTimeLeft) : ""
                    doneBytes: page.sync ? page.sync.downloadDoneBytes : 0
                    leftText: i18ncp("@info files left to download", "%1 file left", "%1 files left", downloading.leftCount)

                    // The thumbnails being downloaded, always said so that nothing moves when
                    // they start or end: their bytes are in the speed, and none of them is a
                    // file counted here.
                    QQC2.Label {
                        objectName: "thumbnailDownloads"
                        Layout.fillWidth: true
                        wrapMode: Text.Wrap
                        font: Kirigami.Theme.smallFont
                        opacity: 0.7
                        text: page.sync ? i18ncp("@info thumbnails of images, videos and documents being downloaded now", "%1 thumbnail downloading",
                                                 "%1 thumbnails downloading", page.sync.thumbnailDownloads) : ""
                    }
                }
                TransferCard {
                    id: uploading
                    objectName: "uploadingCard"
                    visible: page.uploadsShown
                    title: i18nc("@title a mini card", "Uploading")
                    speed: page.sync ? page.sync.uploadSpeed : 0
                    active: page.sync ? page.sync.activeUploads : 0
                    speedHistory: page.sync ? page.sync.uploadSpeedHistory : []
                    activeHistory: page.sync ? page.sync.activeUploadsHistory : []
                    filesText: i18ncp("@info files uploading at once", "%1 file uploading", "%1 files uploading", uploading.active)
                    filesLegend: i18nc("@info chart legend", "Files uploading")
                    leftCount: page.sync ? page.sync.uploadLeftCount : 0
                    leftBytes: page.sync ? page.sync.uploadLeftBytes : 0
                    timeText: page.sync && page.sync.uploadTimeLeft > 0 ? page.timeLeftText(page.sync.uploadTimeLeft) : ""
                    doneBytes: page.sync ? page.sync.uploadDoneBytes : 0
                    leftText: i18ncp("@info changes left to upload", "%1 change left", "%1 changes left", uploading.leftCount)

                    // Why changes wait and go up by themselves; what can't upload is on Issues.
                    Repeater {
                        model: page.waitingRows
                        delegate: QQC2.Label {
                            required property var modelData
                            objectName: "waitingRow"
                            Layout.fillWidth: true
                            wrapMode: Text.Wrap
                            font: Kirigami.Theme.smallFont
                            opacity: 0.7
                            text: i18ncp("@info changes that wait, and why", "1 waits: %2", "%1 wait: %2", modelData.count, modelData.why)
                        }
                    }
                }
            }
            // The account's transfer pool, shared by both ways: shown once, with the large
            // transfers under way, and OneDrive's Retry-After counting down while it runs.
            QQC2.Label {
                objectName: "transferPool"
                Layout.fillWidth: true
                Layout.maximumWidth: Kirigami.Units.gridUnit * 30
                Layout.alignment: Qt.AlignHCenter
                Layout.topMargin: Kirigami.Units.smallSpacing
                visible: page.working
                wrapMode: Text.Wrap
                opacity: 0.7
                text: {
                    if (!page.sync) {
                        return "";
                    }
                    const pool = i18nc("@info the account's transfer pool: %1 slots in use now (may be above %2), %2 the pool's size now, %3 large files moving now, %4 the streams of large files now, %5 how many such streams may run at once",
                                       "Pool: %1 of %2 · large files: %3 (%4 of %5 streams)",
                                       page.sync.poolInUse, page.sync.poolSize, page.sync.largeFiles, page.sync.largeTransfers, page.sync.largeLimit);
                    return page.sync.retryAfter > 0
                        ? i18nc("@info the pool line during OneDrive's Retry-After, seconds left", "%1 — OneDrive asked to wait %2 s", pool, page.sync.retryAfter)
                        : pool;
                }
            }

            // The files moving now, each with how far it is.
            FormCard.FormHeader {
                visible: page.working && page.sync.transfers.count > 0
                title: i18nc("@title:group", "Downloading now")
            }
            FormCard.FormCard {
                objectName: "downloadingNow"
                visible: page.working && page.sync.transfers.count > 0

                Repeater {
                    model: page.sync ? page.sync.transfers : null
                    delegate: TransferRow {}
                }
            }
            FormCard.FormHeader {
                visible: page.working && page.sync.uploads.count > 0
                title: i18nc("@title:group", "Uploading now")
            }
            FormCard.FormCard {
                objectName: "uploadingNow"
                visible: page.working && page.sync.uploads.count > 0

                Repeater {
                    model: page.sync ? page.sync.uploads : null
                    delegate: TransferRow {}
                }
            }

            // History: folded until opened; open, a search, and the events below as the rows.
            FormCard.FormCard {
                id: historyCard
                objectName: "historyCard"
                Layout.topMargin: Kirigami.Units.largeSpacing * 2
                visible: page.working

                FormCard.FormButtonDelegate {
                    objectName: "historyToggle"
                    text: i18nc("@action:button what happened to the files", "History")
                    description: page.historyShown ? ""
                                 : page.sync && page.sync.activity.count > 0
                                   ? i18ncp("@info events in History", "1 event", "%1 events", page.sync.activity.count)
                                   : i18n("Nothing yet")
                    icon.name: "view-history"
                    trailingLogo.direction: page.historyShown ? Qt.UpArrow : Qt.DownArrow
                    onClicked: page.historyShown = !page.historyShown
                }
                FormCard.AbstractFormDelegate {
                    visible: page.historyShown && page.sync !== null && page.sync.activity.count > 0
                    background: null
                    contentItem: Kirigami.SearchField {
                        objectName: "historySearch"
                        placeholderText: i18nc("@info:placeholder", "Search by name or folder…")
                        text: page.searchText
                        onTextEdited: page.searchText = text
                    }
                }
                FormCard.FormPlaceholderMessageDelegate {
                    objectName: "nothingYet"
                    visible: page.historyShown && (page.sync === null || page.sync.activity.count === 0)
                    text: i18n("Nothing yet")
                    explanation: i18n("Downloads, uploads, freed-up files and changes from OneDrive appear here.")
                    icon.name: "view-history"
                }
                FormCard.FormPlaceholderMessageDelegate {
                    objectName: "nothingFound"
                    visible: page.historyShown && page.sync !== null && page.sync.activity.count > 0 && list.count === 0
                    text: i18n("Nothing matches “%1”", page.searchText)
                    icon.name: "edit-find"
                }
            }
        }

        delegate: FormCardRow {
            id: row

            required property string name
            required property string path
            required property string what
            required property string detailText
            required property string iconName
            required property string timeText

            objectName: "historyRow"
            implicitHeight: entry.implicitHeight

            FormCard.FormButtonDelegate {
                id: entry
                anchors.left: parent.left
                anchors.right: parent.right
                text: row.name
                icon.name: row.iconName
                description: row.detailText.length > 0 ? i18nc("@info what happened, detail, when", "%1: %2 · %3", row.what, row.detailText, row.timeText)
                                                        : i18nc("@info what happened, when", "%1 · %2", row.what, row.timeText)
                onClicked: page.sync.showInFolder(row.path)
            }
        }

        footer: Item {
            width: ListView.view.width
            implicitHeight: Kirigami.Units.largeSpacing * 2
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

    Kirigami.PromptDialog {
        id: unpinAllDialog
        objectName: "unpinAllDialog"

        /// The account it was opened for (an AccountItem); null once that account is gone.
        property QtObject item: null

        function confirm() {
            if (item) {
                item.sync.unpinAll();
            }
            close();
        }

        onAboutToShow: item = Current.item
        title: i18nc("@title:window", "Unpin All?")
        subtitle: item
                  ? i18np("1 item (%2) is no longer always kept on this device. Its files stay downloaded until you free up space.",
                          "%1 items (%2) are no longer always kept on this device. Their files stay downloaded until you free up space.",
                          item.sync.pinnedCount, Qt.locale().formattedDataSize(item.sync.pinnedBytes))
                  : ""
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: i18nc("@action:button", "Unpin All")
                icon.name: "window-unpin"
                enabled: unpinAllDialog.item !== null
                onTriggered: unpinAllDialog.confirm()
            },
            Kirigami.Action {
                text: i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: unpinAllDialog.close()
            }
        ]

        // Another account shown, or this one gone: not the question asked.
        Connections {
            target: Current
            function onChanged() {
                unpinAllDialog.close();
            }
        }
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
