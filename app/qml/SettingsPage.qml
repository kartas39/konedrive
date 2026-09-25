import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// The whole app's settings: start at login, download progress, Places,
/// and Quit. Each account's own
/// things (its folder, its sign-in) are on its Account page.
FormCard.FormCardPage {
    id: page

    readonly property bool available: Daemon.serviceAvailable

    objectName: "settingsPage"
    title: i18nc("@title", "Settings")

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: Daemon.actionError
        visible: text.length > 0
    }

    // The app itself
    FormCard.FormHeader {
        title: i18nc("@title:group", "App")
    }
    FormCard.FormCard {
        FormCard.FormSwitchDelegate {
            id: startAtLogin
            objectName: "startAtLogin"
            text: i18n("Start at login")
            description: i18n("KOneDrive starts in the system tray when you log in. Notifications come from it, so with it closed nothing notifies.")
            checked: Autostart.enabled
            onToggled: {
                Autostart.enabled = checked;
                // The entry is the truth: after a failure the switch goes back.
                checked = Qt.binding(() => Autostart.enabled);
            }
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            Layout.margins: Kirigami.Units.smallSpacing
            type: Kirigami.MessageType.Error
            text: Autostart.error
            visible: text.length > 0
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            id: showDownloadProgress
            objectName: "showDownloadProgress"
            text: i18n("Show download and upload progress")
            description: i18n("A long download or upload shows its progress in Plasma, the same way Dolphin's copying does.")
            checked: DownloadProgress.enabled
            onToggled: DownloadProgress.enabled = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            id: showInPlaces
            objectName: "showInPlaces"
            text: i18n("Show in Places")
            description: i18n("Each account's OneDrive folder gets an entry named after the account in Dolphin's Places panel and in file dialogs.")
            checked: Places.enabled
            onToggled: Places.enabled = checked
        }
    }

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing * 2

        FormCard.FormButtonDelegate {
            objectName: "quitButton"
            text: i18nc("@action:button", "Quit KOneDrive")
            description: i18n("Closes the window and the tray icon. Your files stay; nothing notifies until KOneDrive starts again.")
            icon.name: "application-exit"
            onClicked: Qt.quit()
        }
    }
}
