import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The account's mode on OneDrive Settings: two-way sync or read-only,
/// with Change…, and a switch under way with its ways out — the sign-in it waits for in the
/// browser, or the changes read-only waits to send.
FormCard.FormCard {
    id: card

    readonly property var account: Current.account
    readonly property var sync: Current.sync
    readonly property var window: QQC2.ApplicationWindow.window
    /// The account syncs two-way.
    readonly property bool twoWay: account !== null && account.mode === "read-write"
    /// And the folder does: one that cannot be watched runs read-only for now (Folder.Writable).
    readonly property bool uploading: twoWay && sync !== null && sync.writable
    readonly property bool switchingToReadOnly: account !== null && account.switchingToReadOnly

    FormCard.FormTextDelegate {
        objectName: "modeLine"
        text: {
            if (card.switchingToReadOnly) {
                return i18np("Switching to read-only: 1 change still to send", "Switching to read-only: %1 changes still to send", card.account.unsentCount);
            }
            if (card.account !== null && card.account.modeSignInPending) {
                return i18n("Switching to two-way sync");
            }
            return card.uploading ? i18n("Two-way sync") : card.twoWay ? i18n("Read-only for now") : i18n("Read-only");
        }
        description: {
            if (card.switchingToReadOnly) {
                return i18n("The folder turns read-only by itself once they are uploaded.");
            }
            if (card.account !== null && card.account.modeSignInPending) {
                return i18n("Waiting for you to sign in in your browser and allow KOneDrive to change your files.");
            }
            if (card.account !== null && card.account.switchingTo === "read-write") {
                return i18n("Starting the sign-in…");
            }
            if (card.uploading) {
                return i18n("What you do in the folder is uploaded to OneDrive.");
            }
            if (card.twoWay) {
                return i18n("Two-way sync is on, but the folder can't be watched right now, so nothing is uploaded. If it stays so, Status says why.");
            }
            return i18n("Files can't be changed here, and nothing is uploaded.");
        }
        leading: Kirigami.Icon {
            source: card.uploading ? "folder-sync" : "object-locked"
            implicitWidth: Kirigami.Units.iconSizes.medium
            implicitHeight: Kirigami.Units.iconSizes.medium
        }
        trailing: QQC2.Button {
            objectName: "changeModeButton"
            text: i18nc("@action:button", "Change…")
            enabled: card.account !== null && card.account.state === "signed-in" && card.account.switchingTo.length === 0 && !card.switchingToReadOnly
                     && !card.account.modeSignInPending
            onClicked: card.window.changeMode()
        }
    }
    FormCard.FormButtonDelegate {
        visible: card.account !== null && card.account.modeSignInPending
        text: i18nc("@action:button", "Copy Sign-In Link")
        icon.name: "edit-copy"
        onClicked: card.account.copySignInUrl()
    }
    FormCard.FormButtonDelegate {
        objectName: "cancelModeSwitch"
        visible: card.account !== null && card.account.modeSignInPending
        text: i18nc("@action:button", "Cancel")
        icon.name: "dialog-cancel"
        onClicked: card.account.cancelModeSwitch()
    }
    FormCard.FormButtonDelegate {
        objectName: "switchToReadOnlyNow"
        visible: card.switchingToReadOnly
        enabled: card.account !== null && card.account.switchingTo.length === 0
        text: i18nc("@action:button", "Switch Now")
        description: i18n("What is not uploaded stays on this device, and is uploaded once two-way sync is on again.")
        icon.name: "object-locked"
        onClicked: card.account.setMode("read-only", true)
    }
    FormCard.FormButtonDelegate {
        objectName: "cancelReadOnlySwitch"
        visible: card.switchingToReadOnly
        enabled: card.account !== null && card.account.switchingTo.length === 0
        text: i18nc("@action:button", "Keep Two-Way Sync")
        icon.name: "dialog-cancel"
        onClicked: card.account.setMode("read-write", false)
    }
}
