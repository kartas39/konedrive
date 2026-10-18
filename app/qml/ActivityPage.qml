import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.kde.kitemmodels as KItemModels
import org.konedrive.app

/// What happened in the folder of the OneDrive chosen in the sidebar, the latest first: as
/// many events as the daemon keeps (ActivityModel.Capacity). The search keeps those whose
/// path holds what is typed; a click shows the file in the file manager. What moves now is
/// on Status.
ListPage {
    id: page

    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window
    /// What the search keeps: events whose path holds it, in any case.
    property string searchText: ""

    objectName: "activityPage"
    title: window ? window.accountTitle(i18nc("@title", "Activity")) : i18nc("@title", "Activity")

    // A search is in this account's events.
    onSyncChanged: searchText = ""

    ListView {
        id: list

        objectName: "activityList"
        reuseItems: true
        model: KItemModels.KSortFilterProxyModel {
            sourceModel: page.sync ? page.sync.activity : null
            filterRoleName: "path"
            filterString: page.searchText
            filterCaseSensitivity: Qt.CaseInsensitive
        }

        header: ColumnLayout {
            width: ListView.view.width
            spacing: 0

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                Layout.topMargin: Kirigami.Units.largeSpacing
                Layout.leftMargin: Kirigami.Units.largeSpacing
                Layout.rightMargin: Kirigami.Units.largeSpacing
                type: Kirigami.MessageType.Error
                text: page.sync ? page.sync.actionError : ""
                visible: text.length > 0
            }

            Kirigami.SearchField {
                objectName: "activitySearch"
                Layout.alignment: Qt.AlignHCenter
                Layout.fillWidth: true
                Layout.maximumWidth: Kirigami.Units.gridUnit * 30
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.sync !== null && page.sync.activity.count > 0
                placeholderText: i18nc("@info:placeholder", "Search by name or folder…")
                text: page.searchText
                onTextEdited: page.searchText = text
            }

            FormCard.FormCard {
                objectName: "nothingYet"
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.sync === null || page.sync.activity.count === 0

                FormCard.FormPlaceholderMessageDelegate {
                    text: i18n("Nothing yet")
                    explanation: i18n("Downloads, uploads, freed-up files and changes from OneDrive appear here.")
                    icon.name: "view-history"
                }
            }

            FormCard.FormCard {
                objectName: "nothingFound"
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.sync !== null && page.sync.activity.count > 0 && list.count === 0

                FormCard.FormPlaceholderMessageDelegate {
                    text: i18n("Nothing matches “%1”", page.searchText)
                    icon.name: "edit-find"
                }
            }

            Item {
                implicitHeight: Kirigami.Units.largeSpacing
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

            objectName: "activityRow"
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
            implicitHeight: Kirigami.Units.largeSpacing
        }
    }
}
