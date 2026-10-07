import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// What is in OneDrive but not in the folder, each with why. The first
/// `shownLimit` entries are listed — a ListView, so only the rows in sight are
/// built — and then how many more there are and where to see them all. A
/// ScrollablePage rather than a FormCardPage, whose one long column would
/// build every row; the background and the card-coloured rows match it.
Kirigami.ScrollablePage {
    id: page

    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window
    /// How many entries are listed; the rest are counted.
    readonly property int shownLimit: 200
    /// The skippedCount the list was last asked for at.
    property var shownCount: -1
    /// The entries listed (the first `shownLimit`), and how many there are in all.
    property var shown: []
    property int total: 0

    /// Asks for the list when the page is shown, when another account is
    /// chosen while it is, and (through `reload`, at most once a second) when
    /// the count changes while it is.
    function load() {
        if (visible && sync) {
            reload.stop();
            shownCount = sync.skippedCount;
            sync.loadSkipped();
        }
    }

    /// Takes the controller's list. Rows that have not changed are left as
    /// they are, and otherwise the list stays at the row it showed on top.
    function take() {
        const all = sync ? sync.skipped : [];
        total = all.length;
        const first = all.slice(0, shownLimit);
        if (JSON.stringify(first) === JSON.stringify(shown)) {
            return;
        }
        const top = list.indexAt(0, list.contentY);
        shown = first;
        if (top > 0 && first.length > 0) {
            list.positionViewAtIndex(Math.min(top, first.length - 1), ListView.Beginning);
        }
    }

    objectName: "skippedPage"
    // Kept out of the page stack the page takes its implicit height, which
    // would otherwise be the whole list's: the ListView would build every row
    // there and keep them once shown.
    implicitHeight: Kirigami.Units.gridUnit * 20
    title: window ? window.accountTitle(i18nc("@title", "Not in the Folder")) : i18nc("@title", "Not in the Folder")

    topPadding: 0
    bottomPadding: 0
    leftPadding: 0
    rightPadding: 0

    // As FormCardPage's.
    background: Rectangle {
        Kirigami.Theme.colorSet: Kirigami.Theme.Window
        Kirigami.Theme.inherit: false

        Item {
            id: view

            Kirigami.Theme.colorSet: Kirigami.Theme.View
            Kirigami.Theme.inherit: false
        }

        color: Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, view.Kirigami.Theme.backgroundColor, 0.5)
    }

    onVisibleChanged: load()
    onSyncChanged: {
        take();
        load();
    }

    // Each listing page moves the count: the list is asked for again a
    // second after the first change, however many follow in that second.
    Timer {
        id: reload
        interval: 1000
        onTriggered: {
            if (page.sync && page.sync.skippedCount !== page.shownCount) {
                page.load();
            }
        }
    }
    Connections {
        target: page.sync
        function onSkippedChanged() {
            page.take();
        }
        function onSyncChanged() {
            if (page.visible && page.sync.skippedCount !== page.shownCount && !reload.running) {
                reload.start();
            }
        }
    }

    ListView {
        id: list

        objectName: "skippedList"
        model: page.shown
        reuseItems: true

        header: ColumnLayout {
            width: ListView.view.width
            spacing: 0

            // A header that grows or shrinks (the placeholder giving way to
            // the list, a message coming or going) while it is in sight
            // would leave the list off its top: back to the top.
            onHeightChanged: {
                if (ListView.view && ListView.view.contentY < 0) {
                    ListView.view.positionViewAtBeginning();
                }
            }

            FormCard.FormCard {
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.total === 0

                FormCard.FormPlaceholderMessageDelegate {
                    text: i18n("Everything is in the folder")
                    explanation: i18n("Items OneDrive has that cannot be shown here — names too long for Linux, the Personal Vault, shared folders, OneNote notebooks — are listed here with the reason.")
                    icon.name: "view-hidden"
                }
            }
            FormCard.FormSectionText {
                visible: page.total > 0
                text: i18n("These are in your OneDrive but not in the folder. What is inside a skipped folder is covered by the folder's own entry.")
            }
        }

        // One entry, on a card-coloured band as wide as a FormCard.
        delegate: Item {
            id: row

            required property var modelData

            objectName: "skippedRow"
            width: ListView.view.width
            implicitHeight: entry.implicitHeight

            Rectangle {
                Kirigami.Theme.colorSet: Kirigami.Theme.View
                Kirigami.Theme.inherit: false
                anchors.horizontalCenter: parent.horizontalCenter
                width: Math.min(parent.width, Kirigami.Units.gridUnit * 30)
                height: parent.height
                color: Kirigami.Theme.backgroundColor

                FormCard.FormTextDelegate {
                    id: entry
                    anchors.left: parent.left
                    anchors.right: parent.right
                    text: row.modelData.path
                    description: row.modelData.why
                    textItem.elide: Text.ElideMiddle
                }
            }
        }

        footer: ColumnLayout {
            width: ListView.view.width
            spacing: 0

            FormCard.FormCard {
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.total > page.shown.length

                FormCard.FormTextDelegate {
                    objectName: "skippedMore"
                    text: i18np("and 1 more", "and %1 more", page.total - page.shown.length)
                    description: i18n("The full list: konedrivectl sync skipped")
                }
            }
            Item {
                implicitHeight: Kirigami.Units.largeSpacing
            }
        }
    }
}
