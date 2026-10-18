import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// What needs the user, for the account chosen in the sidebar, in three groups: the changes
/// that can't upload, by what can be done about them (UploadQueue.NotUploadedSummary(): a
/// reason one action fixes is one line with its button, files are listed only where each
/// needs something, and only once their reason is opened, at most perFileCap of them); the
/// files whose both versions were kept; and what OneDrive has that can't be shown in the
/// folder. The last two are counted here and listed on pages of their own. What waits and
/// goes up by itself is on Activity.
FormCard.FormCardPage {
    id: page

    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window
    /// What can't upload: the summary without what waits and goes up by itself.
    readonly property var summary: sync ? sync.notUploadedSummary.filter(row => row.group !== "waiting") : []
    /// The counts the summary was last asked for at.
    property string shownCounts: ""
    /// The per-file reasons opened.
    property var opened: ({})
    readonly property bool nothing: summary.length === 0 && (sync === null || (sync.conflictCount === 0 && sync.skippedCount === 0))

    function countsNow() {
        return sync ? sync.pendingCount + "/" + sync.blockedCount + "/" + sync.quotaFull + "/" + sync.spaceWaitingCount + "/" + sync.tooBigCount : "";
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

    function setOpened(reason, open) {
        const next = Object.assign({}, opened);
        next[reason] = open;
        opened = next;
        sync.setNotUploadedFilesShown(reason, open);
    }

    function sizeText(bytes) {
        return bytes > 0 ? Qt.locale().formattedDataSize(bytes) : "";
    }

    objectName: "issuesPage"
    title: window ? window.accountTitle(i18nc("@title", "Issues")) : i18nc("@title", "Issues")

    onVisibleChanged: load()
    onSyncChanged: {
        opened = {};
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

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: page.sync ? page.sync.actionError : ""
        visible: text.length > 0
    }

    FormCard.FormCard {
        objectName: "nothingCard"
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: page.nothing

        FormCard.FormPlaceholderMessageDelegate {
            text: i18n("Nothing needs you")
            explanation: i18n("Changes that can't upload, files whose two versions were both kept, and items that can't be shown in the folder are listed here.")
            icon.name: "checkmark"
        }
    }

    // Can't upload: one action fixes every file of the reason.
    FormCard.FormHeader {
        visible: page.summary.length > 0
        title: i18nc("@title:group", "Can't Upload")
    }
    FormCard.FormCard {
        objectName: "oneActionGroup"
        visible: page.rowsOf("one-action").length > 0

        Repeater {
            model: page.rowsOf("one-action")
            delegate: ColumnLayout {
                id: oneAction
                required property var modelData
                /// OneDrive has no room: its website is where space is made.
                readonly property bool space: modelData.reason === "quota-exceeded" || modelData.reason === "waiting-for-space" || modelData.reason === "too-big"
                Layout.fillWidth: true
                spacing: 0
                FormCard.FormTextDelegate {
                    text: i18np("1 change: %2", "%1 changes: %2", oneAction.modelData.count, oneAction.modelData.why)
                    description: page.sizeText(oneAction.modelData.bytes)
                    textItem.wrapMode: Text.Wrap
                    leading: Kirigami.Icon {
                        source: "dialog-warning"
                        implicitWidth: Kirigami.Units.iconSizes.medium
                        implicitHeight: Kirigami.Units.iconSizes.medium
                    }
                }
                FormCard.FormButtonDelegate {
                    visible: oneAction.space
                    text: i18nc("@action:button", "Open OneDrive on the Web")
                    description: i18n("Delete or move files there to make room.")
                    icon.name: "internet-services"
                    onClicked: page.sync.openOnWeb()
                }
                FormCard.FormButtonDelegate {
                    visible: oneAction.space || oneAction.modelData.reason === "forbidden"
                    text: i18nc("@action:button", "Check Again")
                    description: oneAction.modelData.reason === "forbidden" ? i18n("If OneDrive allows these changes now, they go up.") : i18n("Once there is room in OneDrive, these go up.")
                    icon.name: "view-refresh"
                    onClicked: page.sync.refresh()
                }
            }
        }
    }

    // Can't upload: each file needs something, listed once its reason is opened.
    FormCard.FormCard {
        objectName: "perFileGroup"
        Layout.topMargin: page.rowsOf("one-action").length > 0 ? Kirigami.Units.largeSpacing : 0
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

    // Both versions kept: the list is a page of its own.
    FormCard.FormHeader {
        visible: page.sync !== null && page.sync.conflictCount > 0
        title: i18nc("@title:group", "Kept Both Versions")
    }
    FormCard.FormCard {
        objectName: "conflictsGroup"
        visible: page.sync !== null && page.sync.conflictCount > 0

        FormCard.FormButtonDelegate {
            objectName: "conflictsLink"
            text: page.sync ? i18np("1 file changed here and in OneDrive", "%1 files changed here and in OneDrive", page.sync.conflictCount) : ""
            description: i18n("OneDrive's version keeps the name; yours is the copy beside it.")
            icon.name: "document-duplicate"
            onClicked: page.window.showPage("conflicts")
        }
    }

    // Can't be shown in the folder: the list is a page of its own.
    FormCard.FormHeader {
        visible: page.sync !== null && page.sync.skippedCount > 0
        title: i18nc("@title:group", "Can't Be Shown in the Folder")
    }
    FormCard.FormCard {
        objectName: "skippedGroup"
        visible: page.sync !== null && page.sync.skippedCount > 0

        FormCard.FormButtonDelegate {
            objectName: "skippedLink"
            text: page.sync ? i18np("1 item in OneDrive", "%1 items in OneDrive", page.sync.skippedCount) : ""
            description: i18n("Names too long for Linux, the Personal Vault, shared folders, OneNote notebooks…")
            icon.name: "view-hidden"
            onClicked: page.window.showPage("skipped")
        }
    }
}
