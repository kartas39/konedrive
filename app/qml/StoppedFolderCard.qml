import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// A folder whose syncing stopped (moved, deleted, its disk gone), on Status: why, with Try
/// Again; how many changes made here are not uploaded, which stay in it as plain files; and
/// Use Another Folder…, which opens the dialog that picks a folder: the one that stopped is
/// forgotten, and the one picked registered (FolderChoice).
FormCard.FormCard {
    id: card

    readonly property var sync: Current.sync
    /// The account has a folder, and its syncing stopped.
    readonly property bool stopped: sync !== null && sync.rootPath.length > 0 && sync.overallReason === "stopped"
    /// How many changes made here are not uploaded in the folder that stopped, read when
    /// the card shows it; 0 until then.
    property double unsentCount: 0
    /// Opens the dialog that picks the account's folder.
    signal chooseFolderRequested()

    /// The changes not uploaded in the folder that stopped, asked for again.
    function readUnsent() {
        unsentCount = 0;
        if (visible && stopped) {
            sync.readUnsentCount();
        }
    }

    objectName: "stoppedFolderCard"
    onVisibleChanged: readUnsent()
    onStoppedChanged: readUnsent()

    // Another account: its own folder's changes.
    Connections {
        target: Current
        function onChanged() {
            card.readUnsent();
        }
    }
    Connections {
        target: card.sync
        function onUnsentCountRead(count) {
            card.unsentCount = count;
        }
    }

    // What stopped, and Try Again: a folder that is back syncs on.
    FormCard.FormTextDelegate {
        objectName: "stoppedLine"
        text: i18n("Syncing stopped")
        description: card.sync ? card.sync.trouble : ""
        textItem.wrapMode: Text.Wrap
        descriptionItem.wrapMode: Text.Wrap
        leading: Kirigami.Icon {
            source: "dialog-warning"
            implicitWidth: Kirigami.Units.iconSizes.medium
            implicitHeight: Kirigami.Units.iconSizes.medium
        }
        trailing: QQC2.Button {
            objectName: "tryAgainButton"
            text: i18nc("@action:button", "Try Again")
            icon.name: "view-refresh"
            onClicked: card.sync.refresh()
        }
    }
    FormCard.FormDelegateSeparator {}
    FormCard.FormTextDelegate {
        text: i18n("Or use another folder")
        description: card.sync ? i18n("KOneDrive forgets %1 and leaves whatever is there as it is. Your OneDrive shows up in the empty folder you choose, and files download again when you open them.", card.sync.rootPathText) : ""
        textItem.wrapMode: Text.Wrap
        descriptionItem.wrapMode: Text.Wrap
        leading: Kirigami.Icon {
            source: "folder-cloud"
            implicitWidth: Kirigami.Units.iconSizes.medium
            implicitHeight: Kirigami.Units.iconSizes.medium
        }
    }
    FormCard.FormTextDelegate {
        objectName: "unsentLine"
        visible: card.unsentCount > 0
        text: i18np("1 change made here is not uploaded yet: it stays in the old folder only, as a plain file.",
                    "%1 changes made here are not uploaded yet: they stay in the old folder only, as plain files.",
                    card.unsentCount)
        textItem.wrapMode: Text.Wrap
        leading: Kirigami.Icon {
            source: "dialog-warning"
            implicitWidth: Kirigami.Units.iconSizes.small
            implicitHeight: Kirigami.Units.iconSizes.small
        }
    }
    FormCard.FormDelegateSeparator {}
    FormCard.FormButtonDelegate {
        objectName: "useAnotherFolderButton"
        text: i18nc("@action:button", "Use Another Folder…")
        icon.name: "folder-open"
        onClicked: card.chooseFolderRequested()
    }
}
