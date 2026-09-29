import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// Local versions the sync moved out of the way: where each was, where it is
/// now, when; "Show in Folder" and "Dismiss". A file changed on both sides
/// keeps a copy beside it instead: "Show Both" and "Dismiss". The model holds
/// the newest ConflictModel.Shown (200); a ListView builds only the rows in
/// sight, and a line after them counts the rest. A ScrollablePage rather than
/// a FormCardPage, whose one long column would build every row; the
/// background and the card-coloured rows match it.
Kirigami.ScrollablePage {
    id: page

    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window
    readonly property var conflicts: sync ? sync.conflicts : null

    objectName: "conflictsPage"
    // Kept out of the page stack the page takes its implicit height, which
    // would otherwise be the whole list's: the ListView would build every row
    // there and keep them once shown.
    implicitHeight: Kirigami.Units.gridUnit * 20
    title: window ? window.accountTitle(i18nc("@title", "Conflicts")) : i18nc("@title", "Conflicts")

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

    ListView {
        id: list

        objectName: "conflictsList"
        model: page.conflicts
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
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.conflicts === null || page.conflicts.count === 0

                FormCard.FormPlaceholderMessageDelegate {
                    text: i18n("No conflicts")
                    explanation: i18n("When a file you changed here also changes in OneDrive, your version is kept — moved out of the way, or as a copy beside it while uploading is on — and listed here.")
                    icon.name: "document-duplicate"
                }
            }
            FormCard.FormSectionText {
                visible: page.conflicts !== null && page.conflicts.count > 0
                text: i18n("These files changed in OneDrive while you had changed them here. Your version is kept where it says. Dismiss takes it off this list; the files stay.")
            }
        }

        // One conflict, on a card-coloured band as wide as a FormCard.
        delegate: Item {
            id: row

            required property string name
            required property string original
            required property string originalFolder
            required property string rescued
            required property string rescuedName
            required property bool isCopy
            required property var time

            objectName: "conflictRow"
            width: ListView.view.width
            implicitHeight: entry.implicitHeight

            Rectangle {
                Kirigami.Theme.colorSet: Kirigami.Theme.View
                Kirigami.Theme.inherit: false
                anchors.horizontalCenter: parent.horizontalCenter
                width: Math.min(parent.width, Kirigami.Units.gridUnit * 30)
                height: parent.height
                color: Kirigami.Theme.backgroundColor

                FormCard.AbstractFormDelegate {
                    id: entry
                    anchors.left: parent.left
                    anchors.right: parent.right
                    background: null
                    contentItem: ColumnLayout {
                        spacing: Kirigami.Units.smallSpacing
                        QQC2.Label {
                            Layout.fillWidth: true
                            text: row.name
                            font.bold: true
                            elide: Text.ElideMiddle
                        }
                        QQC2.Label {
                            Layout.fillWidth: true
                            text: row.isCopy ? i18n("In %1", row.originalFolder) : i18n("Was in %1", row.originalFolder)
                            elide: Text.ElideMiddle
                            opacity: 0.8
                        }
                        QQC2.Label {
                            Layout.fillWidth: true
                            text: row.isCopy ? i18n("Changed here and in OneDrive: OneDrive's version keeps the name, yours is %1", row.rescuedName)
                                             : i18n("Now at %1", row.rescued)
                            wrapMode: row.isCopy ? Text.Wrap : Text.WrapAnywhere
                            opacity: 0.8
                        }
                        QQC2.Label {
                            Layout.fillWidth: true
                            text: row.isCopy ? i18n("Kept %1", page.window.when(row.time)) : i18n("Moved %1", page.window.when(row.time))
                            opacity: 0.7
                        }
                        RowLayout {
                            QQC2.Button {
                                visible: row.isCopy
                                text: i18nc("@action:button", "Show Both")
                                icon.name: "document-open-folder"
                                onClicked: page.sync.showBoth(row.original, row.rescued)
                            }
                            QQC2.Button {
                                visible: !row.isCopy
                                text: i18nc("@action:button", "Show in Folder")
                                icon.name: "document-open-folder"
                                onClicked: page.sync.showInFolder(row.rescued)
                            }
                            QQC2.Button {
                                text: i18nc("@action:button", "Dismiss")
                                icon.name: "dialog-close"
                                onClicked: page.sync.dismissConflict(row.rescued)
                            }
                        }
                    }
                }
            }
        }

        footer: ColumnLayout {
            width: ListView.view.width
            spacing: 0

            FormCard.FormCard {
                Layout.topMargin: Kirigami.Units.largeSpacing
                visible: page.conflicts !== null && page.conflicts.total > page.conflicts.count

                FormCard.FormTextDelegate {
                    objectName: "conflictsMore"
                    text: page.conflicts ? i18np("and 1 more", "and %1 more", page.conflicts.total - page.conflicts.count) : ""
                    description: i18n("The full list: konedrivectl sync conflicts")
                }
            }
            Item {
                implicitHeight: Kirigami.Units.largeSpacing
            }
        }
    }
}
