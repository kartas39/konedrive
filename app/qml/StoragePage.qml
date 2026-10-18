import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The space the OneDrive chosen in the sidebar takes: on this device, with what is always
/// kept here and Free Up Space; the free space its downloads leave (FreeSpaceReserve, the
/// daemon's, written to config.toml); and how much of OneDrive is used.
FormCard.FormCardPage {
    id: page

    readonly property var account: Current.account
    readonly property var sync: Current.sync
    // Current.account and Current.sync change together, but these bindings
    // may see one before the other: each checks both.
    readonly property bool hasFolder: account !== null && sync !== null && account.serviceAvailable && sync.serviceAvailable && sync.rootPath.length > 0
    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "storagePage"
    title: window ? window.accountTitle(i18nc("@title", "Storage")) : i18nc("@title", "Storage")

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: page.sync ? page.sync.actionError : ""
        visible: page.hasFolder && text.length > 0
    }

    // On this device.
    FormCard.FormHeader {
        visible: page.hasFolder
        title: i18nc("@title:group", "On This Device")
    }
    FormCard.FormCard {
        objectName: "onThisDevice"
        visible: page.hasFolder

        FormCard.FormTextDelegate {
            objectName: "localBytesLine"
            text: i18n("%1 on this device", Qt.locale().formattedDataSize(page.sync ? page.sync.localBytes : 0))
            description: page.sync && page.sync.pinnedCount > 0
                         ? i18np("Files you open stay downloaded until you free up their space. 1 item is always kept on this device.",
                                 "Files you open stay downloaded until you free up their space. %1 items are always kept on this device.",
                                 page.sync.pinnedCount)
                         : i18n("Files you open stay downloaded until you free up their space.")
            textItem.wrapMode: Text.Wrap
            trailing: QQC2.Button {
                objectName: "freeUpButton"
                text: i18nc("@action:button", "Free Up Space…")
                icon.name: "edit-clear"
                enabled: page.sync !== null && !page.sync.freeingUp
                onClicked: freeUpDialog.open()
            }
        }
        FormCard.AbstractFormDelegate {
            objectName: "freeingUpRow"
            visible: page.sync !== null && page.sync.freeingUp
            background: null
            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing
                QQC2.BusyIndicator {
                    running: page.sync !== null && page.sync.freeingUp
                    implicitWidth: Kirigami.Units.iconSizes.medium
                    implicitHeight: Kirigami.Units.iconSizes.medium
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    text: i18n("Freeing up space…")
                }
            }
        }
        Kirigami.InlineMessage {
            id: freeUpMessage
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            // Not bound: the close button hides it, and the next result shows it again.
            visible: page.sync !== null && page.sync.freeUpResult.length > 0
            type: Kirigami.MessageType.Information
            text: page.sync ? page.sync.freeUpResult : ""
            showCloseButton: true

            Connections {
                target: page.sync
                function onFreeUpResultChanged() {
                    freeUpMessage.visible = page.sync.freeUpResult.length > 0;
                }
            }
            // Another account's result is its own.
            Connections {
                target: Current
                function onChanged() {
                    freeUpMessage.visible = page.sync !== null && page.sync.freeUpResult.length > 0;
                }
            }
        }
        FormCard.FormDelegateSeparator {}
        // The control shows what the daemon says, and a change goes through it.
        FormCard.FormSpinBoxDelegate {
            id: freeSpaceReserveBox
            objectName: "freeSpaceReserveBox"

            readonly property int kept: page.sync !== null ? page.sync.freeSpaceReserve : 0

            label: i18n("Keep at least this much free space on this device, in GiB")
            from: 0
            to: 100000
            value: kept
            // The inner box writes `value` back, ending the binding: kept in step here.
            onKeptChanged: value = kept
            onValueChanged: {
                if (page.sync !== null && value !== kept) {
                    page.sync.setFreeSpaceReserve(value);
                }
            }
        }
        FormCard.FormTextDelegate {
            objectName: "freeSpaceReserveLine"
            text: i18n("A file always kept on this device is not downloaded while its download would leave less free space than this.")
            textItem.wrapMode: Text.Wrap
        }
    }

    // In OneDrive.
    FormCard.FormHeader {
        visible: page.hasFolder && page.account.quotaTotal > 0
        title: i18nc("@title:group", "In OneDrive")
    }
    FormCard.FormCard {
        objectName: "quotaCard"
        visible: page.hasFolder && page.account.quotaTotal > 0

        FormCard.AbstractFormDelegate {
            background: null
            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing

                QQC2.Label {
                    objectName: "quotaLine"
                    Layout.fillWidth: true
                    text: page.account ? i18n("%1 of %2 used",
                                              Qt.locale().formattedDataSize(page.account.quotaUsed),
                                              Qt.locale().formattedDataSize(page.account.quotaTotal)) : ""
                }
                QQC2.ProgressBar {
                    Layout.fillWidth: true
                    from: 0
                    to: page.account ? Math.max(1, page.account.quotaTotal) : 1
                    value: page.account ? page.account.quotaUsed : 0
                }
            }
        }
    }

    Kirigami.PromptDialog {
        id: freeUpDialog
        objectName: "freeUpDialog"

        /// The account it was opened for (an AccountItem); null once that account is gone.
        property QtObject item: null

        function confirm() {
            if (item) {
                item.sync.freeUpSpace();
            }
            close();
        }

        onAboutToShow: item = Current.item
        title: i18nc("@title:window", "Free Up Space?")
        subtitle: i18n("Every downloaded file in the folder that is not open right now becomes online-only again, except what is always kept on this device and what waits to upload. Nothing is deleted from OneDrive; a file downloads again when you open it.")
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: i18nc("@action:button", "Free Up Space")
                icon.name: "edit-clear"
                enabled: freeUpDialog.item !== null
                onTriggered: freeUpDialog.confirm()
            },
            Kirigami.Action {
                text: i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: freeUpDialog.close()
            }
        ]

        // Another account shown, or this one gone: not the question asked.
        Connections {
            target: Current
            function onChanged() {
                freeUpDialog.close();
            }
        }
    }
}
