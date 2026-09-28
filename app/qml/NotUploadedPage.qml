import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// What stays on this computer and is not uploaded, grouped by what can be
/// done about it (Sync1's NotUploadedSummary()): a reason one action fixes is
/// one line with its button; files are listed only where something can be
/// done to each, and only when their reason is opened (NotUploadedFiles(),
/// at most perFileCap of them).
FormCard.FormCardPage {
    id: page

    readonly property var sync: Current.sync
    readonly property var account: Current.account
    readonly property var window: QQC2.ApplicationWindow.window
    readonly property var summary: sync ? sync.notUploadedSummary : []
    /// The counts the summary was last asked for at.
    property string shownCounts: ""
    /// The per-file reasons opened, and whether the waiting ones are.
    property var opened: ({})
    property bool waitingOpened: false

    function countsNow() {
        return sync ? sync.pendingCount + "/" + sync.blockedCount + "/" + sync.heldCount + "/" + sync.quotaFull + "/" + sync.spaceWaitingCount + "/" + sync.tooBigCount : "";
    }

    /// Loads the summary when the page is shown, when a count changes while
    /// it is, and when another account is chosen while it is.
    function load() {
        if (visible && sync) {
            shownCounts = countsNow();
            sync.loadNotUploaded();
        }
    }

    function rowsOf(group) {
        return summary.filter(row => row.group === group);
    }

    function countOf(group) {
        return rowsOf(group).reduce((n, row) => n + row.count, 0);
    }

    function setOpened(reason, open) {
        const next = Object.assign({}, opened);
        next[reason] = open;
        opened = next;
        sync.setNotUploadedFilesShown(reason, open);
    }

    function sizeText(bytes) {
        return bytes > 0 ? Qt.locale().formattedDataSize(bytes) : "";
    }

    objectName: "notUploadedPage"
    title: window ? window.accountTitle(i18nc("@title", "Not Uploaded")) : i18nc("@title", "Not Uploaded")

    onVisibleChanged: load()
    onSyncChanged: {
        opened = {};
        waitingOpened = false;
        load();
    }
    Connections {
        target: page.sync
        enabled: page.visible
        function onSyncChanged() {
            if (page.countsNow() !== page.shownCounts) {
                page.load();
            }
        }
        function onServiceAvailableChanged() {
            page.load();
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.summary.length === 0

        FormCard.FormPlaceholderMessageDelegate {
            text: i18n("Nothing is kept back")
            explanation: i18n("Changes made here that cannot be uploaded — a name OneDrive refuses, a full OneDrive — and what is never uploaded, such as symbolic links, are listed here with the reason.")
            icon.name: "cloud-upload"
        }
    }

    // Needs you: one action fixes every file of the reason.
    FormCard.FormHeader {
        visible: page.rowsOf("one-action").length > 0
        title: i18nc("@title:group", "Needs You")
    }
    FormCard.FormCard {
        objectName: "oneActionGroup"
        visible: page.rowsOf("one-action").length > 0

        Repeater {
            model: page.rowsOf("one-action")
            delegate: ColumnLayout {
                required property var modelData
                Layout.fillWidth: true
                spacing: 0
                FormCard.FormTextDelegate {
                    text: i18np("1 change: %2", "%1 changes: %2", modelData.count, modelData.why)
                    description: page.sizeText(modelData.bytes)
                    leading: Kirigami.Icon {
                        source: "dialog-warning"
                        implicitWidth: Kirigami.Units.iconSizes.medium
                        implicitHeight: Kirigami.Units.iconSizes.medium
                    }
                }
                FormCard.FormButtonDelegate {
                    visible: modelData.reason === "quota-exceeded" || modelData.reason === "waiting-for-space" || modelData.reason === "too-big"
                    text: i18nc("@action:button", "Refresh")
                    description: i18n("Once there is room in OneDrive, these go up.")
                    icon.name: "view-refresh"
                    onClicked: page.sync.refresh()
                }
                FormCard.FormButtonDelegate {
                    visible: modelData.reason === "forbidden" && page.account !== null
                    text: i18nc("@action:button", "Sign In Again")
                    icon.name: "go-next"
                    onClicked: {
                        page.account.signIn();
                        page.window.showPage("account");
                    }
                }
            }
        }
    }

    // Needs you: each file, listed only when its reason is opened.
    FormCard.FormHeader {
        visible: page.rowsOf("per-file").length > 0
        title: i18nc("@title:group", "Needs You for Each File")
    }
    FormCard.FormCard {
        objectName: "perFileGroup"
        visible: page.rowsOf("per-file").length > 0

        Repeater {
            model: page.rowsOf("per-file")
            delegate: ColumnLayout {
                id: reasonBlock
                required property var modelData
                readonly property bool open: page.opened[modelData.reason] === true
                readonly property var files: page.sync ? page.sync.notUploadedFiles[modelData.reason] : undefined
                readonly property var items: open && files ? files.items : []
                readonly property int total: files ? files.total : 0
                Layout.fillWidth: true
                spacing: 0
                FormCard.FormButtonDelegate {
                    text: i18np("1 file: %2", "%1 files: %2", reasonBlock.modelData.count, reasonBlock.modelData.why)
                    icon.name: reasonBlock.open ? "go-down" : "go-next"
                    onClicked: page.setOpened(reasonBlock.modelData.reason, !reasonBlock.open)
                }
                Repeater {
                    model: reasonBlock.items
                    delegate: FormCard.FormButtonDelegate {
                        required property var modelData
                        text: modelData.path
                        // The service's own words for a refused one.
                        description: modelData.reason !== reasonBlock.modelData.reason ? modelData.why : i18n("Show in folder")
                        icon.name: "document-open-folder"
                        onClicked: page.sync.showInFolder(modelData.path)
                    }
                }
                FormCard.FormTextDelegate {
                    visible: reasonBlock.open && reasonBlock.total > reasonBlock.items.length
                    text: i18np("and 1 more", "and %1 more", reasonBlock.total - reasonBlock.items.length)
                    description: i18n("The full list: konedrivectl sync not-uploaded --all")
                }
            }
        }
    }

    // Never uploaded: one line per reason, nothing to do.
    FormCard.FormHeader {
        visible: page.rowsOf("never").length > 0
        title: i18nc("@title:group", "Never Uploaded")
    }
    FormCard.FormCard {
        objectName: "neverGroup"
        visible: page.rowsOf("never").length > 0

        Repeater {
            model: page.rowsOf("never")
            delegate: FormCard.FormTextDelegate {
                required property var modelData
                text: i18np("1 item: %2", "%1 items: %2", modelData.count, modelData.why)
            }
        }
    }

    // Waiting: goes up by itself; one line, its reasons when opened.
    FormCard.FormHeader {
        visible: page.rowsOf("waiting").length > 0
        title: i18nc("@title:group", "Waiting")
    }
    FormCard.FormCard {
        objectName: "waitingGroup"
        visible: page.rowsOf("waiting").length > 0

        FormCard.FormButtonDelegate {
            text: i18np("1 change waits and will go up by itself", "%1 changes wait and will go up by themselves", page.countOf("waiting"))
            icon.name: page.waitingOpened ? "go-down" : "go-next"
            onClicked: page.waitingOpened = !page.waitingOpened
        }
        Repeater {
            model: page.waitingOpened ? page.rowsOf("waiting") : []
            delegate: FormCard.FormTextDelegate {
                required property var modelData
                text: i18np("1: %2", "%1: %2", modelData.count, modelData.why)
            }
        }
    }
}
