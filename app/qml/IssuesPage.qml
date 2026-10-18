import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import QtQml.Models
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// What needs the user, for the account chosen in the sidebar, in one list of three sections.
/// Can't Upload: the changes that can't upload, by what can be done about them
/// (UploadQueue.NotUploadedSummary(): a reason one action fixes is one line with its button,
/// files are listed only where each needs something, and only once their reason is opened,
/// at most perFileCap of them). Kept Both Versions: the files changed here and in OneDrive,
/// each with the copy of your version beside it, Show Both and Dismiss. Can't Be Shown in the
/// Folder: what OneDrive has that the folder can't show, and why. The last two list their
/// first rows (ConflictModel.Shown, `shownLimit`) and count the rest. What waits and goes up
/// by itself is on Status.
///
/// Can't Upload, a few lines, heads the list, and the two long lists are its rows.
ListPage {
    id: page

    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window
    readonly property var conflicts: sync ? sync.conflicts : null
    /// What can't upload: the summary without what waits and goes up by itself.
    readonly property var summary: sync ? sync.notUploadedSummary.filter(row => row.group !== "waiting") : []
    /// The per-file reasons opened.
    property var opened: ({})
    /// How many entries of Can't Be Shown in the Folder are listed; the rest are counted.
    readonly property int shownLimit: 200
    /// The rows after Can't Upload, each {section, kind, …}: a section ("keptBothVersions",
    /// "notInFolder") is its heading, its entries ("conflict", "skipped"), then "more".
    property var rows: []
    /// The counts the summary was last asked for at.
    property string shownCounts: ""
    /// The skippedCount the skipped list was last asked for at.
    property var shownSkipped: -1
    /// The section showSection() asked for, until it is brought to the top.
    property string wantedSection: ""
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

    /// Asks for the skipped list when the page is shown, when another account is chosen
    /// while it is, and (through `skippedReload`, at most once a second) when the count
    /// changes while it is.
    function loadSkipped() {
        if (visible && sync) {
            skippedReload.stop();
            shownSkipped = sync.skippedCount;
            sync.loadSkipped();
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

    /// Brings a section to the top: Can't Upload, which heads the list, or the heading of Kept
    /// Both Versions or of Can't Be Shown in the Folder ("cantUpload", "keptBothVersions",
    /// "notInFolder"). A section whose rows are still on their way is brought once they come,
    /// while the page is shown. A heading at the top stays there when the header above it
    /// grows: the rows' places do not depend on it.
    function showSection(name) {
        wantedSection = name;
        showWanted();
    }

    function showWanted() {
        if (wantedSection === "cantUpload") {
            list.positionViewAtBeginning();
            wantedSection = "";
            return;
        }
        const heading = rows.findIndex(row => row.section === wantedSection && row.kind === "heading");
        if (heading >= 0) {
            list.positionViewAtIndex(heading, ListView.Beginning);
            wantedSection = "";
        }
    }

    /// Takes the conflicts and the skipped list into `rows`. Rows that have not changed are
    /// left as they are, and otherwise the list stays at the row it showed on top.
    function take() {
        const next = [];
        const conflictRows = conflicts ? conflicts.rows() : [];
        if (conflictRows.length > 0) {
            next.push({ section: "keptBothVersions", kind: "heading" });
            for (const row of conflictRows) {
                next.push(Object.assign({ section: "keptBothVersions", kind: "conflict" }, row));
            }
            if (conflicts.total > conflictRows.length) {
                next.push({ section: "keptBothVersions", kind: "more", count: conflicts.total - conflictRows.length });
            }
        }
        const skipped = sync ? sync.skipped : [];
        if (skipped.length > 0) {
            next.push({ section: "notInFolder", kind: "heading" });
            for (const row of skipped.slice(0, shownLimit)) {
                next.push({ section: "notInFolder", kind: "skipped", path: row.path, why: row.why });
            }
            if (skipped.length > shownLimit) {
                next.push({ section: "notInFolder", kind: "more", count: skipped.length - shownLimit });
            }
        }
        if (JSON.stringify(next) === JSON.stringify(rows)) {
            return;
        }
        const top = list.indexAt(0, list.contentY);
        rows = next;
        if (top > 0 && next.length > 0) {
            list.positionViewAtIndex(Math.min(top, next.length - 1), ListView.Beginning);
        }
        if (wantedSection !== "") {
            showWanted();
        }
    }

    objectName: "issuesPage"
    title: window ? window.accountTitle(i18nc("@title", "Issues")) : i18nc("@title", "Issues")

    onVisibleChanged: {
        // A section asked for is brought while the page is shown, not the next time it is.
        if (!visible) {
            wantedSection = "";
        }
        load();
        loadSkipped();
    }
    onSyncChanged: {
        opened = {};
        take();
        load();
        loadSkipped();
    }

    // Each listing page moves the skipped count: the list is asked for again a
    // second after the first change, however many follow in that second.
    Timer {
        id: skippedReload
        interval: 1000
        onTriggered: {
            if (page.sync && page.sync.skippedCount !== page.shownSkipped) {
                page.loadSkipped();
            }
        }
    }
    Connections {
        target: page.sync
        function onSkippedChanged() {
            page.take();
        }
        function onSyncChanged() {
            if (!page.visible) {
                return;
            }
            if (page.countsNow() !== page.shownCounts) {
                page.load();
            }
            if (page.sync.skippedCount !== page.shownSkipped && !skippedReload.running) {
                skippedReload.start();
            }
        }
        function onServiceAvailableChanged() {
            page.load();
        }
    }
    // A dismissed conflict may leave the count as it was, with the next one in its place.
    Connections {
        target: page.conflicts
        function onModelReset() {
            Qt.callLater(page.take);
        }
        function onRowsInserted() {
            Qt.callLater(page.take);
        }
        function onRowsRemoved() {
            Qt.callLater(page.take);
        }
        function onDataChanged() {
            Qt.callLater(page.take);
        }
    }

    ListView {
        id: list

        objectName: "issuesList"
        model: page.rows
        reuseItems: true

        header: ColumnLayout {
            width: ListView.view.width
            spacing: 0

            // A header that grows or shrinks (a reason opened, a message coming or going)
            // while it is in sight would leave the list off its top: back to the top.
            onHeightChanged: {
                if (ListView.view && ListView.view.contentY < 0) {
                    ListView.view.positionViewAtBeginning();
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
                    text: i18n("Nothing needs your attention")
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
        }

        delegate: DelegateChooser {
            role: "kind"

            DelegateChoice {
                roleValue: "heading"
                delegate: SectionHeading {}
            }
            DelegateChoice {
                roleValue: "conflict"
                delegate: ConflictRow {}
            }
            DelegateChoice {
                roleValue: "skipped"
                delegate: SkippedRow {}
            }
            DelegateChoice {
                roleValue: "more"
                delegate: MoreRow {}
            }
        }

        footer: Item {
            width: ListView.view.width
            implicitHeight: Kirigami.Units.largeSpacing
        }
    }

    /// The head of Kept Both Versions or of Can't Be Shown in the Folder, and what it lists.
    component SectionHeading: ColumnLayout {
        id: heading

        required property var modelData

        width: ListView.view ? ListView.view.width : 0
        spacing: 0

        FormCard.FormHeader {
            Layout.fillWidth: true
            title: heading.modelData.section === "keptBothVersions" ? i18nc("@title:group", "Kept Both Versions") : i18nc("@title:group", "Can't Be Shown in the Folder")
        }
        FormCard.FormSectionText {
            Layout.fillWidth: true
            text: heading.modelData.section === "keptBothVersions"
                  ? i18n("These files changed in OneDrive while you had changed them here. OneDrive's version keeps the name; yours is the copy beside it. Dismiss takes it off this list; the files stay.")
                  : i18n("These are in your OneDrive but can't be shown in the folder: names too long for Linux, the Personal Vault, shared folders, OneNote notebooks… What is inside such a folder is covered by the folder's own entry.")
        }
    }

    /// One file whose both versions were kept: where, under which name, when.
    component ConflictRow: FormCardRow {
        id: row

        required property var modelData

        objectName: "conflictRow"
        implicitHeight: conflictEntry.implicitHeight

        FormCard.AbstractFormDelegate {
            id: conflictEntry
            anchors.left: parent.left
            anchors.right: parent.right
            background: null
            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                QQC2.Label {
                    Layout.fillWidth: true
                    text: row.modelData.name
                    font.bold: true
                    elide: Text.ElideMiddle
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    text: i18n("In %1", row.modelData.originalFolder)
                    elide: Text.ElideMiddle
                    opacity: 0.8
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    text: i18n("Changed here and in OneDrive: OneDrive's version keeps the name, yours is %1", row.modelData.copyName)
                    wrapMode: Text.Wrap
                    opacity: 0.8
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    text: i18n("Kept %1", row.modelData.timeText)
                    opacity: 0.7
                }
                RowLayout {
                    QQC2.Button {
                        text: i18nc("@action:button", "Show Both")
                        icon.name: "document-open-folder"
                        onClicked: page.sync.showBoth(row.modelData.original, row.modelData.copy)
                    }
                    QQC2.Button {
                        text: i18nc("@action:button", "Dismiss")
                        icon.name: "dialog-close"
                        onClicked: page.sync.dismissConflict(row.modelData.copy)
                    }
                }
            }
        }
    }

    /// One item OneDrive has that can't be shown in the folder, and why.
    component SkippedRow: FormCardRow {
        id: row

        required property var modelData

        objectName: "skippedRow"
        implicitHeight: skippedEntry.implicitHeight

        FormCard.FormTextDelegate {
            id: skippedEntry
            anchors.left: parent.left
            anchors.right: parent.right
            text: row.modelData.path
            description: row.modelData.why
            textItem.elide: Text.ElideMiddle
        }
    }

    /// How many more a section has than it lists, and where to see them all.
    component MoreRow: FormCardRow {
        id: row

        required property var modelData
        readonly property alias text: moreEntry.text
        readonly property alias description: moreEntry.description

        objectName: modelData.section === "keptBothVersions" ? "conflictsMore" : "skippedMore"
        implicitHeight: moreEntry.implicitHeight

        FormCard.FormTextDelegate {
            id: moreEntry
            anchors.left: parent.left
            anchors.right: parent.right
            text: i18np("and 1 more", "and %1 more", row.modelData.count)
            description: row.modelData.section === "keptBothVersions" ? i18n("The full list: konedrivectl sync conflicts")
                                                                        : i18n("The full list: konedrivectl sync skipped")
        }
    }
}
