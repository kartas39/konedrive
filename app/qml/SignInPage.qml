import QtQuick
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.konedrive.app

/// Adding a OneDrive. With no account yet, what adding one means, and Sign In, which opens
/// Microsoft's sign-in in the browser; one more is added from the switcher, which opens it at
/// once (Main.addOneDrive). While it waits, its link can be copied and the sign-in given up;
/// a sign-in that adds nothing says why, with Sign In to try again. Once the account is
/// added, the window shows its Status and the dialog that picks its folder.
FormCard.FormCardPage {
    id: page

    objectName: "signInPage"
    title: i18nc("@title", "Add OneDrive")

    FormCard.FormCard {
        Layout.topMargin: Kirigami.Units.largeSpacing

        FormCard.FormPlaceholderMessageDelegate {
            objectName: "addIntro"
            visible: Accounts.count === 0 && !Accounts.adding
            text: i18n("Add your OneDrive")
            explanation: i18n("Sign in with your Microsoft account in the browser, then choose an empty folder: your OneDrive shows up in it, and files download when you open them.")
            icon.name: "folder-cloud"
        }
        FormCard.FormDelegateSeparator {
            visible: Accounts.count === 0 && !Accounts.adding
        }
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
