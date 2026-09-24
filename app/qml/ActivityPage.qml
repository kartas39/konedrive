import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// Downloads under way, then the recent events; a click shows the file in the file manager.
FormCard.FormCardPage {
    id: page

    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "activityPage"
    title: i18nc("@title", "Activity")

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: Sync.actionError
        visible: text.length > 0
    }

    FormCard.FormHeader {
        visible: Sync.transfers.count > 0
        title: i18nc("@title:group", "Downloading now")
    }
    FormCard.FormCard {
        visible: Sync.transfers.count > 0

        Repeater {
            model: Sync.transfers
            delegate: FormCard.AbstractFormDelegate {
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
                        text: name
                        elide: Text.ElideMiddle
                    }
                    QQC2.ProgressBar {
                        Layout.fillWidth: true
                        from: 0
                        to: 1
                        value: fraction
                        indeterminate: total === 0
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: i18n("%1 of %2", Qt.locale().formattedDataSize(done), Qt.locale().formattedDataSize(total))
                        opacity: 0.7
                    }
                }
            }
        }
    }

    FormCard.FormHeader {
        title: i18nc("@title:group", "Recent")
    }
    FormCard.FormCard {
        FormCard.FormPlaceholderMessageDelegate {
            visible: Sync.activity.count === 0
            text: i18n("Nothing yet")
            explanation: i18n("Downloads, freed-up files and changes from OneDrive appear here.")
            icon.name: "view-history"
        }
        Repeater {
            model: Sync.activity
            delegate: FormCard.FormButtonDelegate {
                required property string name
                required property string path
                required property string what
                required property string detail
                required property string iconName
                required property var time
                text: name
                icon.name: iconName
                description: (detail.length > 0 ? i18nc("@info what happened, detail, when", "%1: %2 · %3", what, detail, page.window.when(time))
                                                 : i18nc("@info what happened, when", "%1 · %2", what, page.window.when(time)))
                onClicked: Sync.showInFolder(path)
            }
        }
    }
}
