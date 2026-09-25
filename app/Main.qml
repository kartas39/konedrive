import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.konedrive.app

import "qml"

/// The window: pages chosen from a sidebar on the left, which folds into a
/// drawer behind a menu button when the window is narrow. The account
/// switcher heads the sidebar; the pages under it show the account chosen
/// there, and Settings, below them, is the whole app's.
Kirigami.ApplicationWindow {
    id: root

    /// The page shown: status, activity, conflicts, skipped, notUploaded, account or settings.
    property string currentPage: "status"
    /// Wide enough for the sidebar beside a page.
    readonly property bool sidebarFits: width >= Kirigami.Units.gridUnit * 36
    /// An account is chosen: the per-account pages have something to show.
    readonly property bool hasAccount: Current.account !== null

    title: i18nc("@title:window", "KOneDrive")
    // main.cpp shows the window unless started with --background.
    visible: false
    width: Kirigami.Units.gridUnit * 46
    height: Kirigami.Units.gridUnit * 34
    minimumWidth: Kirigami.Units.gridUnit * 20
    minimumHeight: Kirigami.Units.gridUnit * 20

    /// Shows one of the pages, by name. With no account, the per-account
    /// pages give way to Status, which says how to add one.
    function showPage(name) {
        const pages = {
            "status": statusPage,
            "activity": activityPage,
            "conflicts": conflictsPage,
            "skipped": skippedPage,
            "notUploaded": notUploadedPage,
            "account": accountPage,
            "settings": settingsPage,
        };
        if (!pages[name]) {
            return;
        }
        if (!hasAccount && name !== "settings") {
            name = "status";
        }
        if (name !== currentPage || pageStack.depth === 0) {
            currentPage = name;
            pageStack.clear();
            pageStack.push(pages[name]);
        }
        if (drawer.modal) {
            drawer.close();
        }
    }

    /// A per-account page's title: with more than one account it names the
    /// account too ("Status · Personal"), since a narrow window hides the switcher.
    function accountTitle(title) {
        if (Accounts.count > 1 && Current.account) {
            return i18nc("@title page title, account label", "%1 · %2", title, Current.account.label);
        }
        return title;
    }

    /// Straight to Microsoft's sign-in in the browser; the dialog only when no
    /// client ID is set yet, to ask for it.
    /// The account an added sign-in still has to choose a folder for.
    property string folderPickerFor: ""

    /// Opens the folder picker for the account just added, once the window
    /// shows it: never for whichever account happened to be shown before.
    function openFolderPickerWhenShown() {
        if (folderPickerFor.length > 0 && Current.path === folderPickerFor) {
            folderPickerFor = "";
            showPage("account");
            accountPage.openFolderPicker();
        }
    }

    function signIn() {
        if (Accounts.adding) {
            return;
        }
        Accounts.clearAddError();
        Accounts.addAccount("");
    }

    /// A unix time as the time of day today, or a short date and time before.
    function when(unixSeconds) {
        const date = new Date(unixSeconds * 1000);
        if (date.toDateString() === new Date().toDateString()) {
            return date.toLocaleTimeString(Qt.locale(), Locale.ShortFormat);
        }
        return date.toLocaleString(Qt.locale(), Locale.ShortFormat);
    }

    Connections {
        target: Accounts
        function onOpenUrlRequested(url) {
            Qt.openUrlExternally(url);
        }
        // Sign In named and chose this account: its page shows it, and a
        // sign-in exists to sync something, so the folder picker opens too.
        function onAccountAdded(path) {
            root.folderPickerFor = path;
            Current.select(path);
            root.openFolderPickerWhenShown();
        }
    }
    Connections {
        target: Current
        function onChanged() {
            root.openFolderPickerWhenShown();
            if (!root.hasAccount && root.currentPage !== "settings") {
                root.showPage("status");
            }
        }
    }

    Component.onCompleted: pageStack.push(statusPage)
    pageStack.globalToolBar.showNavigationButtons: Kirigami.ApplicationHeaderStyle.NoNavigationButtons

    component SidebarEntry: QQC2.ItemDelegate {
        id: entry

        required property string name
        property int badge: 0

        objectName: "sidebar-" + name
        Layout.fillWidth: true
        highlighted: root.currentPage === name
        onClicked: root.showPage(name)

        contentItem: RowLayout {
            spacing: Kirigami.Units.largeSpacing
            Kirigami.Icon {
                source: entry.icon.name
                implicitWidth: Kirigami.Units.iconSizes.smallMedium
                implicitHeight: Kirigami.Units.iconSizes.smallMedium
            }
            QQC2.Label {
                Layout.fillWidth: true
                text: entry.text
                elide: Text.ElideRight
            }
            // The count of conflicts, or of changes that cannot be uploaded, when there are any.
            Rectangle {
                visible: entry.badge > 0
                radius: height / 2
                color: Kirigami.Theme.negativeTextColor
                implicitHeight: badgeLabel.implicitHeight + Kirigami.Units.smallSpacing
                implicitWidth: Math.max(implicitHeight, badgeLabel.implicitWidth + Kirigami.Units.largeSpacing)
                QQC2.Label {
                    id: badgeLabel
                    anchors.centerIn: parent
                    text: entry.badge
                    color: Kirigami.Theme.highlightedTextColor
                    font.bold: true
                }
            }
        }
    }

    globalDrawer: Kirigami.GlobalDrawer {
        id: drawer

        objectName: "sidebar"
        modal: !root.sidebarFits
        handleVisible: modal
        width: Kirigami.Units.gridUnit * 12
        leftPadding: 0
        rightPadding: 0
        topPadding: Kirigami.Units.smallSpacing
        // A sidebar is always open; a drawer opens from its button.
        onModalChanged: drawerOpen = !modal
        Component.onCompleted: drawerOpen = !modal

        header: AccountSwitcher {
            onAddRequested: root.signIn()
        }

        // The pages of the account chosen above.
        Repeater {
            model: [
                { name: "status", text: i18nc("@title sidebar", "Status"), icon: Current.status ? Current.status.iconName : "state-offline" },
                { name: "activity", text: i18nc("@title sidebar", "Activity"), icon: "view-history" },
                { name: "conflicts", text: i18nc("@title sidebar", "Conflicts"), icon: "document-duplicate" },
                { name: "skipped", text: i18nc("@title sidebar", "Not in the Folder"), icon: "view-hidden" },
                { name: "notUploaded", text: i18nc("@title sidebar", "Not Uploaded"), icon: "cloud-upload" },
                { name: "account", text: i18nc("@title sidebar", "Account"), icon: "im-user" },
            ]
            delegate: SidebarEntry {
                required property var modelData
                name: modelData.name
                text: modelData.text
                icon.name: modelData.icon
                enabled: root.hasAccount || modelData.name === "status"
                badge: {
                    if (!Current.sync) {
                        return 0;
                    }
                    if (modelData.name === "conflicts") {
                        return Current.sync.conflictCount;
                    }
                    return modelData.name === "notUploaded" ? Current.sync.blockedCount : 0;
                }
            }
        }
        Kirigami.Separator {
            Layout.fillWidth: true
            Layout.topMargin: Kirigami.Units.smallSpacing
            Layout.bottomMargin: Kirigami.Units.smallSpacing
        }
        // The whole app's.
        SidebarEntry {
            name: "settings"
            text: i18nc("@title sidebar", "Settings")
            icon.name: "settings-configure"
        }
        Item {
            Layout.fillHeight: true
        }
    }


    // The pages live for the window's lifetime: the page row borrows the one
    // shown and hands it back here (hidden) when another is chosen.
    Item {
        visible: false

        StatusPage {
            id: statusPage
        }
        ActivityPage {
            id: activityPage
        }
        ConflictsPage {
            id: conflictsPage
        }
        SkippedPage {
            id: skippedPage
        }
        NotUploadedPage {
            id: notUploadedPage
        }
        AccountPage {
            id: accountPage
        }
        SettingsPage {
            id: settingsPage
        }
    }
}
