import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// Signing in and out, the account and its quota — or that the service is not running.
FormCard.FormCardPage {
    id: page

    readonly property bool available: Account.serviceAvailable
    readonly property bool signedOut: available && Account.state === "signed-out"
    readonly property bool signingIn: available && Account.state === "signing-in"
    readonly property bool signedIn: available && Account.state === "signed-in"
    readonly property string errorText: Account.actionError.length > 0 ? Account.actionError : Account.lastError
    readonly property var window: QQC2.ApplicationWindow.window

    objectName: "accountPage"
    title: i18nc("@title", "Account")

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.largeSpacing
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        type: Kirigami.MessageType.Error
        text: page.errorText
        visible: page.available && text.length > 0
    }

    // Daemon not reachable
    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing
        visible: !page.available

        FormCard.FormTextDelegate {
            text: i18n("The KOneDrive service is not running")
            description: i18n("Install it with scripts/dev-install.sh, then try again.")
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Try Again")
            icon.name: "view-refresh"
            // M8: the daemon that came back also owns Sync1 — both re-fetch.
            onClicked: {
                Account.retry();
                Sync.retry();
            }
        }
    }

    // Signed out
    FormCard.FormHeader {
        visible: page.signedOut
        title: i18nc("@title:group", "Sign In")
    }
    FormCard.FormCard {
        visible: page.signedOut

        FormCard.FormTextDelegate {
            visible: Account.clientId.length === 0
            text: i18n("A client ID is needed first")
            description: i18n("Register an application in Microsoft Entra and enter its Application (client) ID in Settings, under Advanced. README.md explains the steps.")
        }
        FormCard.FormButtonDelegate {
            visible: Account.clientId.length === 0
            text: i18nc("@action:button", "Open Settings")
            icon.name: "settings-configure"
            onClicked: page.window.showPage("settings")
        }
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Sign In to OneDrive")
            icon.name: "go-next"
            enabled: Account.clientId.length > 0
            onClicked: Account.signIn()
        }
    }

    // Signing in
    FormCard.FormHeader {
        visible: page.signingIn
        title: i18nc("@title:group", "Signing In")
    }
    FormCard.FormCard {
        visible: page.signingIn

        FormCard.FormTextDelegate {
            text: i18n("Finish signing in in your browser")
            description: Account.signInUrl
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Copy Sign-In Link")
            icon.name: "edit-copy"
            enabled: Account.signInUrl.length > 0
            onClicked: Account.copySignInUrl()
        }
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onClicked: Account.cancelSignIn()
        }
    }

    // Signed in
    FormCard.FormHeader {
        visible: page.signedIn
        title: i18nc("@title:group", "Account")
    }
    FormCard.FormCard {
        visible: page.signedIn

        FormCard.FormTextDelegate {
            text: Account.displayName.length > 0 ? Account.displayName : i18n("Loading…")
            description: Account.email
        }
        FormCard.FormDelegateSeparator {}
        FormCard.AbstractFormDelegate {
            background: null
            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing

                QQC2.Label {
                    Layout.fillWidth: true
                    text: i18n("%1 of %2 used",
                               Qt.locale().formattedDataSize(Account.quotaUsed),
                               Qt.locale().formattedDataSize(Account.quotaTotal))
                }
                QQC2.ProgressBar {
                    Layout.fillWidth: true
                    from: 0
                    to: Math.max(1, Account.quotaTotal)
                    value: Account.quotaUsed
                }
            }
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Refresh")
            icon.name: "view-refresh"
            onClicked: Account.refreshAccountInfo()
        }
        FormCard.FormButtonDelegate {
            text: i18nc("@action:button", "Sign Out")
            icon.name: "system-log-out"
            onClicked: Account.signOut()
        }
    }
}
