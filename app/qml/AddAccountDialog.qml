import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.konedrive.app

/// Add Account: the client ID first when none is set (the same field as in
/// Settings, checked by the daemon the same way), then the account's name;
/// then, in one step, the account is added, chosen, and its sign-in opens in
/// the browser. The Account page follows the sign-in from there.
Kirigami.PromptDialog {
    id: dialog

    readonly property bool needsClientId: Daemon.clientId.length === 0
    readonly property string name: labelField.text.trim()
    readonly property string labelProblem: Accounts.labelProblem(labelField.text)
    readonly property bool ready: !Accounts.adding && labelProblem.length === 0 && (!needsClientId || clientIdField.text.trim().length > 0)

    function add() {
        if (ready) {
            Accounts.addAccount(labelField.text, needsClientId ? clientIdField.text : "");
        }
    }

    objectName: "addAccountDialog"
    title: i18nc("@title:window", "Add Account")
    subtitle: needsClientId
        ? i18n("KOneDrive signs in with an application you register in Microsoft Entra: enter its Application (client) ID. README.md explains the steps.")
        : i18n("Name the account, then sign in to it in your browser.")
    preferredWidth: Kirigami.Units.gridUnit * 26
    // The explanation wraps rather than widening the dialog to the window.
    maximumWidth: Math.min(absoluteMaximumWidth, Kirigami.Units.gridUnit * 30)
    standardButtons: Kirigami.Dialog.NoButton
    onOpened: {
        Accounts.clearAddError();
        clientIdField.text = "";
        labelField.text = Accounts.suggestedLabel();
        if (needsClientId) {
            clientIdField.forceActiveFocus();
        } else {
            labelField.forceActiveFocus();
        }
    }

    Kirigami.InlineMessage {
        Layout.fillWidth: true
        type: Kirigami.MessageType.Error
        text: Accounts.addError
        visible: text.length > 0
    }

    Kirigami.FormLayout {
        Layout.fillWidth: true

        QQC2.TextField {
            id: clientIdField
            objectName: "clientIdField"
            visible: dialog.needsClientId
            Layout.fillWidth: true
            Kirigami.FormData.label: i18nc("@label:textbox", "Application (client) ID:")
            placeholderText: "00000000-0000-0000-0000-000000000000"
            onAccepted: labelField.forceActiveFocus()
        }
        QQC2.TextField {
            id: labelField
            objectName: "labelField"
            Layout.fillWidth: true
            Kirigami.FormData.label: i18nc("@label:textbox", "Name:")
            placeholderText: i18nc("@info:placeholder", "Personal, Family…")
            onAccepted: dialog.add()
        }
    }
    QQC2.Label {
        readonly property bool problem: dialog.name.length > 0 && dialog.labelProblem.length > 0

        Layout.fillWidth: true
        wrapMode: Text.Wrap
        font: Kirigami.Theme.smallFont
        color: problem ? Kirigami.Theme.negativeTextColor : Kirigami.Theme.disabledTextColor
        text: problem ? dialog.labelProblem
                      : i18n("Shown in KOneDrive, and in the Places panel as “OneDrive — %1”. You can change it later.",
                             dialog.name.length > 0 ? dialog.name : i18nc("@info an example account name", "Personal"))
    }

    customFooterActions: [
        Kirigami.Action {
            text: i18nc("@action:button", "Add and Sign In…")
            icon.name: "list-add-user"
            enabled: dialog.ready
            onTriggered: dialog.add()
        },
        Kirigami.Action {
            text: i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: dialog.close()
        }
    ]

    Connections {
        target: Accounts
        function onAccountAdded() {
            dialog.close();
        }
    }
}
