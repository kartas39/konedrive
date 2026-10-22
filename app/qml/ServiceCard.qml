import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The KOneDrive service is not running: what that means, a way to start it, and its log
/// to see why it stopped. The window picks the service up by itself once it runs.
FormCard.FormCard {
    id: card

    readonly property bool failed: ServiceUnit.startError.length > 0

    objectName: "serviceCard"

    FormCard.FormTextDelegate {
        text: card.failed ? i18n("The KOneDrive service couldn't start") : i18n("The KOneDrive service isn't running")
        description: card.failed ? ServiceUnit.startError : i18n("Until it runs, your OneDrive folder shows only the files already downloaded, and nothing syncs.")
        textItem.wrapMode: Text.Wrap
        leading: Kirigami.Icon {
            source: card.failed ? "dialog-error" : "state-offline"
            implicitWidth: Kirigami.Units.iconSizes.medium
            implicitHeight: Kirigami.Units.iconSizes.medium
        }
    }
    FormCard.FormDelegateSeparator {}
    FormCard.FormButtonDelegate {
        objectName: "startServiceButton"
        visible: !ServiceUnit.starting
        text: card.failed ? i18nc("@action:button", "Start Again") : i18nc("@action:button", "Start Service")
        icon.name: "media-playback-start"
        onClicked: ServiceUnit.start()
    }
    FormCard.AbstractFormDelegate {
        visible: ServiceUnit.starting
        background: null
        contentItem: RowLayout {
            spacing: Kirigami.Units.largeSpacing
            QQC2.BusyIndicator {
                running: ServiceUnit.starting
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
            QQC2.Label {
                Layout.fillWidth: true
                text: i18n("Starting the service…")
            }
        }
    }
    FormCard.FormButtonDelegate {
        objectName: "serviceLogButton"
        text: i18nc("@action:button", "Show Details")
        icon.name: "view-list-text"
        onClicked: {
            ServiceUnit.readJournal();
            logDialog.open();
        }
    }

    Kirigami.Dialog {
        id: logDialog

        title: i18nc("@title:window", "KOneDrive Service Log")
        standardButtons: Kirigami.Dialog.Close
        preferredWidth: Kirigami.Units.gridUnit * 40
        preferredHeight: Kirigami.Units.gridUnit * 24

        QQC2.ScrollView {
            QQC2.TextArea {
                id: log
                text: ServiceUnit.journal.length > 0 ? ServiceUnit.journal : i18n("Reading the log…")
                readOnly: true
                selectByMouse: true
                wrapMode: TextEdit.Wrap
                font: Kirigami.Theme.fixedWidthFont
            }
        }
        customFooterActions: [
            Kirigami.Action {
                text: i18nc("@action:button", "Copy")
                icon.name: "edit-copy"
                onTriggered: {
                    log.selectAll();
                    log.copy();
                    log.deselect();
                }
            }
        ]
    }
}
