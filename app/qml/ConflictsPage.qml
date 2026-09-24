import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// Local versions the sync moved out of the way: where each was,
/// where it is now, when; "Show in Folder" and "Dismiss".
FormCard.FormCardPage {
    id: page

    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "conflictsPage"
    title: i18nc("@title", "Conflicts")

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: Sync.actionError
        visible: text.length > 0
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing

        FormCard.FormPlaceholderMessageDelegate {
            visible: Sync.conflicts.count === 0
            text: i18n("No conflicts")
            explanation: i18n("When a file you changed here also changes in OneDrive, your version is moved out of the way and listed here.")
            icon.name: "document-duplicate"
        }
        FormCard.FormSectionText {
            visible: Sync.conflicts.count > 0
            text: i18n("These files changed in OneDrive while you had changed them here. Your version was moved out of the way and is kept where it says. Dismiss takes it off this list; the file stays.")
        }
        Repeater {
            model: Sync.conflicts
            delegate: FormCard.AbstractFormDelegate {
                required property string name
                required property string originalFolder
                required property string rescued
                required property var time
                Layout.fillWidth: true
                background: null
                contentItem: ColumnLayout {
                    spacing: Kirigami.Units.smallSpacing
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: name
                        font.bold: true
                        elide: Text.ElideMiddle
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: i18n("Was in %1", originalFolder)
                        elide: Text.ElideMiddle
                        opacity: 0.8
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: i18n("Now at %1", rescued)
                        wrapMode: Text.WrapAnywhere
                        opacity: 0.8
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: i18n("Moved %1", page.window.when(time))
                        opacity: 0.7
                    }
                    RowLayout {
                        QQC2.Button {
                            text: i18nc("@action:button", "Show in Folder")
                            icon.name: "document-open-folder"
                            onClicked: Sync.showInFolder(rescued)
                        }
                        QQC2.Button {
                            text: i18nc("@action:button", "Dismiss")
                            icon.name: "dialog-close"
                            onClicked: Sync.dismissConflict(rescued)
                        }
                    }
                }
            }
        }
    }
}
