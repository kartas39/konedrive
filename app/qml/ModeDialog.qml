import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// How an account's folder syncs: two-way (what is done in the folder is uploaded) or
/// read-only. One dialog for the whole window, opened for the account shown from the
/// Overview and from OneDrive Settings. Two-way goes through the browser, where Microsoft
/// asks to let KOneDrive change the files; read-only first sends what is not sent yet.
Kirigami.Dialog {
    id: dialog

    /// The account it was opened for (an AccountItem); null once that account is gone.
    property QtObject item: null
    /// The choice in the dialog: two-way, or read-only.
    property bool twoWay: false
    /// The account syncs two-way now.
    readonly property bool isTwoWay: item !== null && item.account.mode === "read-write"
    readonly property string folder: item !== null && item.sync.rootPath.length > 0 ? item.sync.rootPath : i18n("the OneDrive folder")

    function openFor(accountItem) {
        item = accountItem;
        twoWay = isTwoWay;
        open();
    }

    function confirm() {
        if (item) {
            item.account.setMode(twoWay ? "read-write" : "read-only", false);
        }
        close();
    }

    objectName: "modeDialog"
    title: i18nc("@title:window", "How Should This Folder Sync?")
    standardButtons: Kirigami.Dialog.NoButton
    maximumWidth: Math.min(absoluteMaximumWidth, Kirigami.Units.gridUnit * 30)
    onClosed: item = null

    ColumnLayout {
        spacing: 0

        FormCard.FormRadioDelegate {
            objectName: "twoWayChoice"
            text: i18n("Two-way sync")
            description: i18n("What you add, change, rename, move or delete in %1 is uploaded to OneDrive and shows on your other devices. Deleted files go to OneDrive's recycle bin.", dialog.folder)
            checked: dialog.twoWay
            onToggled: dialog.twoWay = checked
        }
        FormCard.FormRadioDelegate {
            objectName: "readOnlyChoice"
            text: i18n("Read-only")
            description: i18n("Files from OneDrive arrive and open as usual, but can't be changed here, and nothing is uploaded.")
            checked: !dialog.twoWay
            onToggled: dialog.twoWay = !checked
        }
        QQC2.Label {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.largeSpacing
            visible: dialog.twoWay && !dialog.isTwoWay
            text: i18n("Your browser opens so that you can sign in to Microsoft again, this time allowing KOneDrive to change the files in your OneDrive.")
            wrapMode: Text.Wrap
        }
        QQC2.Label {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.largeSpacing
            visible: !dialog.twoWay && dialog.isTwoWay
            text: i18n("Changes made here that are not uploaded yet are sent first; then the folder turns read-only by itself.")
            wrapMode: Text.Wrap
        }
    }

    customFooterActions: [
        Kirigami.Action {
            text: dialog.twoWay ? i18nc("@action:button", "Continue in Browser") : i18nc("@action:button", "Switch to Read-Only")
            icon.name: dialog.twoWay ? "go-next" : "object-locked"
            // Only once the choice is the other mode: the one it has needs nothing done.
            visible: dialog.item !== null && dialog.twoWay !== dialog.isTwoWay
            onTriggered: dialog.confirm()
        },
        Kirigami.Action {
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: dialog.close()
        }
    ]

    // Another account shown, or this one gone: not the question asked.
    Connections {
        target: Current
        function onChanged() {
            dialog.close();
        }
    }
}
