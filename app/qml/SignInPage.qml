import QtQuick
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// Adding a OneDrive, the first one or one more alike: Sign In opens Microsoft's sign-in in
/// the browser; while it waits, its link can be copied and the sign-in given up; a sign-in
/// that adds nothing says why. Once the account is added, the window shows its Status,
/// where its folder is chosen.
FormCard.FormCardPage {
    id: page

    objectName: "signInPage"
    title: i18nc("@title", "Add OneDrive")

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing

        FormCard.FormPlaceholderMessageDelegate {
            text: i18n("Add your OneDrive")
            explanation: i18n("Sign in with your Microsoft account in the browser. Your OneDrive then shows up in a folder on this device, and files download when you open them.")
            icon.name: "folder-cloud"
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            objectName: "addError"
            visible: Accounts.addError.length > 0
            text: Accounts.addError
            textItem.wrapMode: Text.Wrap
            icon.name: "dialog-error"
        }
        FormCard.FormButtonDelegate {
            objectName: "signInButton"
            visible: !Accounts.adding
            text: i18nc("@action:button", "Sign In…")
            icon.name: "go-next"
            onClicked: {
                Accounts.clearAddError();
                Accounts.addAccount("");
            }
        }
        FormCard.FormTextDelegate {
            objectName: "signInWait"
            visible: Accounts.adding
            text: i18n("Waiting for you to sign in in your browser…")
            description: i18n("If no browser opened, copy the link and open it in one.")
            textItem.wrapMode: Text.Wrap
            icon.name: "view-refresh"
        }
        FormCard.FormButtonDelegate {
            objectName: "copySignInLink"
            visible: Accounts.adding
            enabled: Accounts.signInUrl.length > 0
            text: i18nc("@action:button", "Copy Sign-In Link")
            icon.name: "edit-copy"
            onClicked: Accounts.copySignInUrl()
        }
        FormCard.FormButtonDelegate {
            objectName: "cancelSignIn"
            visible: Accounts.adding
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onClicked: Accounts.cancelAdd()
        }
    }
}
